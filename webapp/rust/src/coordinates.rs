use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use chrono::{DateTime, Utc};
use sqlx::{MySql, MySqlPool, QueryBuilder};
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::{
    notifications::{Audience, NotificationHub},
    Coordinate, Error,
};

struct Update {
    generation: u64,
    id: String,
    chair_id: String,
    coordinate: Coordinate,
    reply: oneshot::Sender<Result<i64, String>>,
}

#[derive(Debug, Clone)]
pub struct CoordinateWriter {
    sender: mpsc::Sender<Update>,
    pub initialization: Arc<RwLock<()>>,
    generation: Arc<AtomicU64>,
}

impl CoordinateWriter {
    pub fn new(pool: MySqlPool, notifications: NotificationHub) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Update>(1024);
        let initialization = Arc::new(RwLock::new(()));
        let generation = Arc::new(AtomicU64::new(0));
        let worker_initialization = initialization.clone();
        let worker_generation = generation.clone();
        tokio::spawn(async move {
            let mut pending = None;
            loop {
                let first = match pending.take() {
                    Some(first) => first,
                    None => match receiver.recv().await {
                        Some(first) => first,
                        None => break,
                    },
                };
                let mut batch = vec![first];
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(1);
                while batch.len() < 64 {
                    let Ok(Some(next)) = tokio::time::timeout_at(deadline, receiver.recv()).await
                    else {
                        break;
                    };
                    // Preserve per-chair ordering even for concurrent callers.
                    if batch.iter().any(|item| item.chair_id == next.chair_id) {
                        pending = Some(next);
                        break;
                    }
                    batch.push(next);
                }
                let _guard = worker_initialization.read().await;
                let generation = worker_generation.load(Ordering::SeqCst);
                batch = batch
                    .into_iter()
                    .filter_map(|item| {
                        if item.generation == generation {
                            Some(item)
                        } else {
                            let _ = item.reply.send(Err("database was initialized".into()));
                            None
                        }
                    })
                    .collect();
                if batch.is_empty() {
                    continue;
                }
                match persist(&pool, &batch).await {
                    Ok((recorded_at, changed)) => {
                        for (user, chair) in changed {
                            notifications.notify(Audience::User(user));
                            notifications.notify(Audience::Chair(chair));
                        }
                        for item in batch {
                            let _ = item.reply.send(Ok(recorded_at));
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "coordinate batch failed");
                        for item in batch {
                            let _ = item.reply.send(Err(error.to_string()));
                        }
                    }
                }
            }
        });
        Self {
            sender,
            initialization,
            generation,
        }
    }

    pub async fn record(&self, chair_id: String, coordinate: Coordinate) -> Result<i64, Error> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Update {
                generation: self.generation.load(Ordering::SeqCst),
                id: ulid::Ulid::new().to_string(),
                chair_id,
                coordinate,
                reply,
            })
            .await
            .map_err(|_| Error::Background("coordinate writer stopped".into()))?;
        response
            .await
            .map_err(|_| Error::Background("coordinate writer stopped".into()))?
            .map_err(Error::Background)
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    // Called while holding the initialization write guard; reject stale queued work.
    pub fn reset(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

#[derive(sqlx::FromRow)]
struct ActiveRide {
    id: String,
    user_id: String,
    chair_id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
    destination_latitude: i32,
    destination_longitude: i32,
    status: String,
}

async fn persist(pool: &MySqlPool, batch: &[Update]) -> sqlx::Result<(i64, Vec<(String, String)>)> {
    let mut tx = pool.begin().await?;
    let recorded_at: DateTime<Utc> = sqlx::query_scalar("SELECT CURRENT_TIMESTAMP(6)")
        .fetch_one(&mut *tx)
        .await?;
    let mut locations = QueryBuilder::<MySql>::new(
        "INSERT INTO chair_locations (id,chair_id,latitude,longitude,created_at) ",
    );
    locations.push_values(batch, |mut row, item| {
        row.push_bind(&item.id)
            .push_bind(&item.chair_id)
            .push_bind(item.coordinate.latitude)
            .push_bind(item.coordinate.longitude)
            .push_bind(recorded_at);
    });
    locations.build().execute(&mut *tx).await?;
    // Each batch has distinct chairs. Keep the aggregate and history in one transaction.
    let mut distances = QueryBuilder::<MySql>::new(
        "INSERT INTO chair_distances (chair_id,latitude,longitude,total_distance,updated_at) ",
    );
    distances.push_values(batch, |mut row, item| {
        row.push_bind(&item.chair_id)
            .push_bind(item.coordinate.latitude)
            .push_bind(item.coordinate.longitude)
            .push("0")
            .push_bind(recorded_at);
    });
    // Assignment order matters: compute the delta using the previous position first.
    distances.push(" ON DUPLICATE KEY UPDATE total_distance=total_distance+ABS(latitude-VALUES(latitude))+ABS(longitude-VALUES(longitude)),latitude=VALUES(latitude),longitude=VALUES(longitude),updated_at=VALUES(updated_at)");
    distances.build().execute(&mut *tx).await?;

    let mut query = QueryBuilder::<MySql>::new(
        "SELECT r.id,r.user_id,r.chair_id,r.pickup_latitude,r.pickup_longitude,
         r.destination_latitude,r.destination_longitude,
         (SELECT status FROM ride_statuses WHERE ride_id=r.id ORDER BY created_at DESC,id DESC LIMIT 1) AS status
         FROM chairs c JOIN rides r ON r.id=(SELECT id FROM rides WHERE chair_id=c.id ORDER BY updated_at DESC,id DESC LIMIT 1)
         WHERE c.id IN ("
    );
    {
        let mut ids = query.separated(",");
        for item in batch {
            ids.push_bind(&item.chair_id);
        }
    }
    query.push(")");
    let rides: Vec<ActiveRide> = query.build_query_as().fetch_all(&mut *tx).await?;
    let mut changes = Vec::new();
    for ride in rides {
        let coordinate = &batch
            .iter()
            .find(|item| item.chair_id == ride.chair_id)
            .unwrap()
            .coordinate;
        let next = if ride.status == "ENROUTE"
            && coordinate.latitude == ride.pickup_latitude
            && coordinate.longitude == ride.pickup_longitude
        {
            Some("PICKUP")
        } else if ride.status == "CARRYING"
            && coordinate.latitude == ride.destination_latitude
            && coordinate.longitude == ride.destination_longitude
        {
            Some("ARRIVED")
        } else {
            None
        };
        if let Some(status) = next {
            changes.push((ulid::Ulid::new().to_string(), ride, status));
        }
    }
    if !changes.is_empty() {
        let mut statuses =
            QueryBuilder::<MySql>::new("INSERT INTO ride_statuses (id,ride_id,status) ");
        statuses.push_values(&changes, |mut row, (id, ride, status)| {
            row.push_bind(id).push_bind(&ride.id).push_bind(status);
        });
        statuses.build().execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok((
        recorded_at.timestamp_millis(),
        changes
            .into_iter()
            .map(|(_, ride, _)| (ride.user_id, ride.chair_id))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires COORDINATE_TEST_DATABASE_URL pointing to isuride_coordinate_test"]
    async fn persists_before_reply_and_transitions_once() -> anyhow::Result<()> {
        let pool = MySqlPool::connect(&std::env::var("COORDINATE_TEST_DATABASE_URL")?).await?;
        let db: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&pool)
            .await?;
        assert_eq!(db, "isuride_coordinate_test");
        sqlx::raw_sql("INSERT INTO chairs (id,owner_id,name,model,is_active,access_token) VALUES ('coordinate-chair','owner','chair','model',TRUE,'token');
          INSERT INTO rides (id,user_id,chair_id,pickup_latitude,pickup_longitude,destination_latitude,destination_longitude) VALUES ('coordinate-ride','user','coordinate-chair',1,2,3,4);
          INSERT INTO ride_statuses (id,ride_id,status,created_at) VALUES ('coordinate-start','coordinate-ride','ENROUTE','2020-01-01');")
            .execute(&pool).await?;
        sqlx::query("INSERT INTO chair_locations (id,chair_id,latitude,longitude,created_at) VALUES ('coordinate-history','coordinate-chair',-1,1,'2020-01-01')").execute(&pool).await?;
        let rebuild = std::fs::read_to_string("/home/isucon/webapp/sql/4-derived-data.sql")?;
        sqlx::raw_sql(&rebuild).execute(&pool).await?;
        sqlx::raw_sql(&rebuild).execute(&pool).await?; // Rebuilding twice must not double-count.
        let writer = CoordinateWriter::new(pool.clone(), Default::default());
        sqlx::raw_sql("CREATE TRIGGER fail_coordinate_status BEFORE INSERT ON ride_statuses FOR EACH ROW SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='injected failure'").execute(&pool).await?;
        let failed = writer
            .record(
                "coordinate-chair".into(),
                Coordinate {
                    latitude: 1,
                    longitude: 2,
                },
            )
            .await;
        sqlx::raw_sql("DROP TRIGGER fail_coordinate_status")
            .execute(&pool)
            .await?;
        assert!(failed.is_err());
        let unchanged:(i64,i32,i32)=sqlx::query_as("SELECT total_distance,latitude,longitude FROM chair_distances WHERE chair_id='coordinate-chair'").fetch_one(&pool).await?;
        assert_eq!(unchanged, (0, -1, 1));
        let history: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chair_locations")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            history, 1,
            "history and total must roll back if the status write fails"
        );
        let first = writer
            .record(
                "coordinate-chair".into(),
                Coordinate {
                    latitude: 1,
                    longitude: 2,
                },
            )
            .await?;
        let saved: DateTime<Utc> = sqlx::query_scalar(
            "SELECT created_at FROM chair_locations WHERE chair_id='coordinate-chair' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(first, saved.timestamp_millis());
        assert_eq!(
            crate::get_latest_ride_status(&pool, "coordinate-ride").await?,
            "PICKUP"
        );
        writer
            .record(
                "coordinate-chair".into(),
                Coordinate {
                    latitude: 1,
                    longitude: 2,
                },
            )
            .await?;
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ride_statuses WHERE status='PICKUP'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 1);
        sqlx::query("INSERT INTO ride_statuses (id,ride_id,status) VALUES ('coordinate-carry','coordinate-ride','CARRYING')").execute(&pool).await?;
        writer
            .record(
                "coordinate-chair".into(),
                Coordinate {
                    latitude: 3,
                    longitude: 4,
                },
            )
            .await?;
        assert_eq!(
            crate::get_latest_ride_status(&pool, "coordinate-ride").await?,
            "ARRIVED"
        );
        let distance:(i64,i32,i32)=sqlx::query_as("SELECT total_distance,latitude,longitude FROM chair_distances WHERE chair_id='coordinate-chair'").fetch_one(&pool).await?;
        assert_eq!(distance, (7, 3, 4)); // (-1,1) -> (1,2) -> (1,2) -> (3,4)
        let before_rebuild = distance;
        sqlx::raw_sql(&rebuild).execute(&pool).await?;
        let rebuilt:(i64,i32,i32)=sqlx::query_as("SELECT total_distance,latitude,longitude FROM chair_distances WHERE chair_id='coordinate-chair'").fetch_one(&pool).await?;
        assert_eq!(before_rebuild, rebuilt);
        let mut tasks = Vec::new();
        for i in 0..32 {
            let writer = writer.clone();
            tasks.push(tokio::spawn(async move {
                writer
                    .record(
                        format!("batch-{i}"),
                        Coordinate {
                            latitude: i,
                            longitude: -i,
                        },
                    )
                    .await
            }));
        }
        for task in tasks {
            task.await??;
        }
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chair_locations")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 36);
        assert!(writer
            .record(
                "x".repeat(27),
                Coordinate {
                    latitude: 0,
                    longitude: 0
                }
            )
            .await
            .is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chair_locations")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 36);
        let guard = writer.initialization.write().await;
        let (reply, response) = oneshot::channel();
        writer
            .sender
            .send(Update {
                generation: writer.generation.load(Ordering::SeqCst),
                id: "stale-coordinate".into(),
                chair_id: "coordinate-chair".into(),
                coordinate: Coordinate {
                    latitude: 10,
                    longitude: 10,
                },
                reply,
            })
            .await?;
        writer.reset();
        drop(guard);
        assert!(response.await?.is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chair_locations")
            .fetch_one(&pool)
            .await?;
        assert_eq!(count, 36);
        Ok(())
    }
}
