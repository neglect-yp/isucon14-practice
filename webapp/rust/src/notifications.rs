use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    http::HeaderMap,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
};
use chrono::{DateTime, Utc};
use futures_util::stream;
use sqlx::{MySqlConnection, MySqlPool};
use tokio::sync::watch;

use crate::{models::Ride, AppState, Error};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Audience {
    User(String),
    Chair(String),
}

impl Audience {
    fn id(&self) -> &str {
        match self {
            Self::User(id) | Self::Chair(id) => id,
        }
    }
    fn owner_column(&self) -> &'static str {
        match self {
            Self::User(_) => "r.user_id",
            Self::Chair(_) => "r.chair_id",
        }
    }
    fn sent_column(&self) -> &'static str {
        match self {
            Self::User(_) => "app_sent_at",
            Self::Chair(_) => "chair_sent_at",
        }
    }
}

// Only wakeups live in memory. Ordered events and delivery progress live in DB.
#[derive(Debug, Default, Clone)]
pub struct NotificationHub(Arc<Mutex<HashMap<Audience, watch::Sender<u64>>>>);

impl NotificationHub {
    fn subscribe(&self, audience: &Audience) -> watch::Receiver<u64> {
        let mut channels = self.0.lock().unwrap();
        channels.retain(|_, sender| sender.receiver_count() > 0);
        channels
            .entry(audience.clone())
            .or_insert_with(|| watch::channel(0).0)
            .subscribe()
    }

    pub fn notify(&self, audience: Audience) {
        let mut channels = self.0.lock().unwrap();
        if let Some(sender) = channels.get(&audience) {
            if sender.receiver_count() > 0 {
                sender.send_modify(|version| *version = version.wrapping_add(1));
            } else {
                channels.remove(&audience);
            }
        }
    }

    pub fn reset(&self) {
        self.0.lock().unwrap().clear();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    ride_created_at: DateTime<Utc>,
    ride_id: String,
    status_created_at: DateTime<Utc>,
    status_id: String,
}

#[derive(sqlx::FromRow)]
pub(crate) struct NotificationRow {
    #[sqlx(flatten)]
    pub ride: Ride,
    pub status_id: String,
    pub status: String,
    pub status_created_at: DateTime<Utc>,
    pub app_sent_at: Option<DateTime<Utc>>,
    pub chair_sent_at: Option<DateTime<Utc>>,
}

impl NotificationRow {
    fn cursor(&self) -> Cursor {
        Cursor {
            ride_created_at: self.ride.created_at,
            ride_id: self.ride.id.clone(),
            status_created_at: self.status_created_at,
            status_id: self.status_id.clone(),
        }
    }
    fn needs_ack(&self, audience: &Audience) -> bool {
        match audience {
            Audience::User(_) => self.app_sent_at.is_none(),
            Audience::Chair(_) => self.chair_sent_at.is_none(),
        }
    }
}

fn select_prefix(audience: &Audience) -> String {
    format!(
        "SELECT r.*, s.id AS status_id, s.status, s.created_at AS status_created_at,
             s.app_sent_at, s.chair_sent_at FROM rides r JOIN ride_statuses s ON s.ride_id=r.id
             WHERE {} = ?",
        audience.owner_column()
    )
}

async fn next_row(
    conn: &mut MySqlConnection,
    audience: &Audience,
    cursor: Option<&Cursor>,
) -> sqlx::Result<Option<NotificationRow>> {
    let prefix = select_prefix(audience);
    let row = if let Some(cursor) = cursor {
        sqlx::query_as::<_, NotificationRow>(&format!(
            "{prefix}
            AND (r.created_at,r.id,s.created_at,s.id) > (?,?,?,?)
            ORDER BY r.created_at,r.id,s.created_at,s.id LIMIT 1"
        ))
        .bind(audience.id())
        .bind(cursor.ride_created_at)
        .bind(&cursor.ride_id)
        .bind(cursor.status_created_at)
        .bind(&cursor.status_id)
        .fetch_optional(&mut *conn)
        .await?
    } else {
        sqlx::query_as::<_, NotificationRow>(&format!(
            "{prefix} AND s.{} IS NULL
            ORDER BY r.created_at,r.id,s.created_at,s.id LIMIT 1",
            audience.sent_column()
        ))
        .bind(audience.id())
        .fetch_optional(&mut *conn)
        .await?
    };
    if row.is_some() {
        return Ok(row);
    }

    if let Some(cursor) = cursor {
        // The same state may acquire a chair or updated statistics after commit.
        sqlx::query_as(&format!("{prefix} AND s.id = ?"))
            .bind(audience.id())
            .bind(&cursor.status_id)
            .fetch_optional(conn)
            .await
    } else {
        // Reconnecting without a Last-Event-ID gets pending history, or latest state.
        sqlx::query_as(&format!(
            "{prefix} ORDER BY r.created_at DESC,r.id DESC,s.created_at DESC,s.id DESC LIMIT 1"
        ))
        .bind(audience.id())
        .fetch_optional(conn)
        .await
    }
}

struct PreparedEvent {
    cursor: Cursor,
    payload: String,
    needs_ack: bool,
}

async fn prepare(
    pool: &MySqlPool,
    audience: &Audience,
    cursor: Option<&Cursor>,
) -> Result<Option<PreparedEvent>, Error> {
    let mut tx = pool.begin().await?;
    let Some(row) = next_row(&mut tx, audience, cursor).await? else {
        tx.commit().await?;
        return Ok(None);
    };
    let payload = match audience {
        Audience::User(_) => crate::app_handlers::notification_payload(&mut tx, &row).await?,
        Audience::Chair(_) => crate::chair_handlers::notification_payload(&mut tx, &row).await?,
    };
    let event = PreparedEvent {
        cursor: row.cursor(),
        payload,
        needs_ack: row.needs_ack(audience),
    };
    tx.commit().await?;
    Ok(Some(event))
}

async fn acknowledge(pool: &MySqlPool, audience: &Audience, status_id: &str) -> sqlx::Result<()> {
    let column = audience.sent_column();
    sqlx::query(&format!(
        "UPDATE ride_statuses SET {column}=CURRENT_TIMESTAMP(6) WHERE id=? AND {column} IS NULL"
    ))
    .bind(status_id)
    .execute(pool)
    .await?;
    Ok(())
}

struct Connection {
    state: AppState,
    audience: Audience,
    updates: watch::Receiver<u64>,
    cursor: Option<Cursor>,
    last_payload: Option<String>,
    pending_ack: Option<String>,
    drain: bool,
}

impl Connection {
    async fn next(&mut self) -> Result<Option<Event>, Error> {
        loop {
            if self.updates.has_changed().is_err() {
                return Ok(None);
            }
            // Record progress only AFTER the previous event was yielded to the
            // response body. Dropping a stream before then leaves it replayable.
            if let Some(id) = self.pending_ack.take() {
                acknowledge(&self.state.pool, &self.audience, &id).await?;
            }
            if !self.drain {
                // DB reconciliation also recovers a lost wakeup or a write from
                // another process. No DB connection/transaction is held while idle.
                if matches!(
                    tokio::time::timeout(Duration::from_secs(1), self.updates.changed()).await,
                    Ok(Err(_))
                ) {
                    return Ok(None);
                }
            }
            self.updates.borrow_and_update();
            let prepared = prepare(&self.state.pool, &self.audience, self.cursor.as_ref()).await?;
            if self.updates.has_changed().is_err() {
                return Ok(None);
            }
            self.drain = false;
            if let Some(prepared) = prepared {
                let changed = self.cursor.as_ref() != Some(&prepared.cursor)
                    || self.last_payload.as_ref() != Some(&prepared.payload);
                if changed {
                    let event = Event::default()
                        .id(&prepared.cursor.status_id)
                        .data(&prepared.payload);
                    if prepared.needs_ack {
                        self.pending_ack = Some(prepared.cursor.status_id.clone());
                    }
                    self.cursor = Some(prepared.cursor);
                    self.last_payload = Some(prepared.payload);
                    self.drain = true;
                    return Ok(Some(event));
                }
            } else if self.last_payload.is_none() {
                self.last_payload = Some("null".into());
                return Ok(Some(Event::default().data("null")));
            }
        }
    }
}

pub async fn respond(
    state: AppState,
    audience: Audience,
    headers: HeaderMap,
) -> Result<Response, Error> {
    // Subscribe before reading DB so a commit during the initial read cannot be lost.
    let updates = state.notifications.subscribe(&audience);
    let cursor = if let Some(id) = headers.get("last-event-id").and_then(|id| id.to_str().ok()) {
        sqlx::query_as::<_, NotificationRow>(&format!("{} AND s.id=?", select_prefix(&audience)))
            .bind(audience.id())
            .bind(id)
            .fetch_optional(&state.pool)
            .await?
            .map(|r| r.cursor())
    } else {
        None
    };
    let connection = Connection {
        state,
        audience,
        updates,
        cursor,
        last_payload: None,
        pending_ack: None,
        drain: true,
    };
    let events = stream::unfold(Some(connection), |connection| async move {
        let mut connection = connection?;
        match connection.next().await {
            Ok(Some(event)) => Some((Ok::<_, Error>(event), Some(connection))),
            Ok(None) => None,
            Err(error) => {
                tracing::error!(%error, "notification stream failed");
                Some((Err(error), None))
            }
        }
    });
    let mut response = Sse::new(events)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(10)))
        .into_response();
    response
        .headers_mut()
        .insert("x-accel-buffering", "no".parse().unwrap());
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wakeups_are_scoped_and_reset_closes_subscriptions() {
        let hub = NotificationHub::default();
        let a = Audience::User("a".into());
        let b = Audience::Chair("a".into());
        let mut first = hub.subscribe(&a);
        let other = hub.subscribe(&b);
        hub.notify(a.clone());
        hub.notify(a);
        first.changed().await.unwrap();
        assert_eq!(*first.borrow_and_update(), 2);
        assert!(!other.has_changed().unwrap());
        hub.reset();
        assert!(first.changed().await.is_err());
        assert!(other.has_changed().is_err());
    }

    struct ClientStream {
        response: reqwest::Response,
        pending: String,
    }

    impl ClientStream {
        async fn next(&mut self) -> anyhow::Result<serde_json::Value> {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Some(end) = self.pending.find("\n\n") {
                        let frame: String = self.pending.drain(..end + 2).collect();
                        if let Some(data) =
                            frame.lines().find_map(|line| line.strip_prefix("data: "))
                        {
                            return Ok(serde_json::from_str(data)?);
                        }
                        continue;
                    }
                    let chunk = self
                        .response
                        .chunk()
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("SSE ended early"))?;
                    self.pending.push_str(std::str::from_utf8(&chunk)?);
                }
            })
            .await?
        }
    }

    async fn connect(
        client: &reqwest::Client,
        base: &str,
        who: &str,
        last_id: Option<&str>,
    ) -> anyhow::Result<ClientStream> {
        let (route, cookie) = match who {
            "user" => ("app", "app_session=sse-user-token"),
            "other" => ("app", "app_session=sse-other-token"),
            _ => ("chair", "chair_session=sse-chair-token"),
        };
        let mut request = client
            .get(format!("{base}/api/{route}/notification"))
            .header("cookie", cookie);
        if let Some(id) = last_id {
            request = request.header("last-event-id", id);
        }
        let response = request.send().await?;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["content-type"], "text/event-stream");
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        Ok(ClientStream {
            response,
            pending: String::new(),
        })
    }

    use axum::http::StatusCode;

    #[tokio::test]
    #[ignore = "requires SSE_TEST_DATABASE_URL pointing to isuride_sse_test"]
    async fn http_stream_orders_history_replays_and_releases_db_connections() -> anyhow::Result<()>
    {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(2)
            .connect(&std::env::var("SSE_TEST_DATABASE_URL")?)
            .await?;
        let db: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&pool)
            .await?;
        assert_eq!(db, "isuride_sse_test");
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await?;
        assert_eq!(users, 0, "test database must be empty");
        sqlx::raw_sql("INSERT INTO users (id,username,firstname,lastname,date_of_birth,access_token,invitation_code) VALUES
          ('sse-user','sse-user','test','user','2000-01-01','sse-user-token','sse-user-invite'),
          ('sse-other','sse-other','test','other','2000-01-01','sse-other-token','sse-other-invite');
          INSERT INTO chairs (id,owner_id,name,model,is_active,access_token) VALUES ('sse-chair','sse-owner','test','test',TRUE,'sse-chair-token');
          INSERT INTO rides (id,user_id,chair_id,pickup_latitude,pickup_longitude,destination_latitude,destination_longitude,evaluation,created_at) VALUES
          ('sse-old','sse-user','sse-chair',0,0,1,1,4,'2020-01-01'),
          ('sse-new','sse-user','sse-chair',1,1,2,2,NULL,'2020-01-02');")
            .execute(&pool).await?;
        let statuses = [
            "MATCHING",
            "ENROUTE",
            "PICKUP",
            "CARRYING",
            "ARRIVED",
            "COMPLETED",
        ];
        for (i, status) in statuses.iter().enumerate() {
            sqlx::query("INSERT INTO ride_statuses (id,ride_id,status,created_at) VALUES (?, 'sse-old', ?, ?)")
                .bind(format!("sse-old-{i}")).bind(status).bind(format!("2020-01-01 00:00:0{i}"))
                .execute(&pool).await?;
        }
        sqlx::query("INSERT INTO ride_statuses (id,ride_id,status,created_at) VALUES ('sse-new-0','sse-new','MATCHING','2020-01-02')")
            .execute(&pool).await?;
        let state = AppState::new(pool.clone());
        let app = axum::Router::new()
            .merge(crate::app_handlers::app_routes(state.clone()))
            .merge(crate::chair_handlers::chair_routes(state.clone()))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let unauthenticated = client
            .get(format!("{base}/api/app/notification"))
            .send()
            .await?;
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
        let mut user = connect(&client, &base, "user", None).await?;
        let mut chair = connect(&client, &base, "chair", None).await?;
        for status in statuses {
            let app = user.next().await?;
            let seat = chair.next().await?;
            assert_eq!(app["ride_id"], "sse-old");
            assert_eq!(seat["ride_id"], "sse-old");
            assert_eq!(app["status"], status);
            assert_eq!(seat["status"], status);
            assert_eq!(app["chair"]["stats"]["total_rides_count"], 1);
            assert_eq!(app["chair"]["stats"]["total_evaluation_avg"], 4.0);
        }
        assert_eq!(user.next().await?["ride_id"], "sse-new");
        assert_eq!(chair.next().await?["status"], "MATCHING");
        // Both SSE connections stay open, but a pool of just two still serves SQL.
        tokio::time::timeout(
            Duration::from_secs(1),
            sqlx::query("SELECT 1").execute(&pool),
        )
        .await??;

        let mut tx = pool.begin().await?;
        for (i, status) in [(1, "ENROUTE"), (2, "PICKUP")] {
            sqlx::query("INSERT INTO ride_statuses (id,ride_id,status,created_at) VALUES (?, 'sse-new', ?, ?)")
                .bind(format!("sse-new-{i}")).bind(status).bind(format!("2020-01-02 00:00:0{i}"))
                .execute(&mut *tx).await?;
        }
        tx.commit().await?;
        state
            .notifications
            .notify(Audience::User("sse-user".into()));
        state
            .notifications
            .notify(Audience::Chair("sse-chair".into()));
        for status in ["ENROUTE", "PICKUP"] {
            assert_eq!(user.next().await?["status"], status);
            assert_eq!(chair.next().await?["status"], status);
        }
        drop(chair);
        let mut replay = connect(&client, &base, "chair", Some("sse-new-0")).await?;
        assert_eq!(replay.next().await?["status"], "ENROUTE");
        assert_eq!(replay.next().await?["status"], "PICKUP");
        let mut other = connect(&client, &base, "other", Some("sse-new-0")).await?;
        assert!(
            other.next().await?.is_null(),
            "Last-Event-ID must not cross users"
        );

        let mut tx = pool.begin().await?;
        sqlx::query("INSERT INTO ride_statuses (id,ride_id,status) VALUES ('rolled-back','sse-new','CARRYING')").execute(&mut *tx).await?;
        tx.rollback().await?;
        state
            .notifications
            .notify(Audience::User("sse-user".into()));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), user.next())
                .await
                .is_err()
        );
        state.notifications.reset();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), user.response.chunk())
                .await??
                .is_none()
        );
        let mut latest = connect(&client, &base, "user", None).await?;
        assert_eq!(latest.next().await?["status"], "PICKUP");
        state.notifications.reset();
        drop((latest, replay, other, user));
        server.abort();
        let _ = server.await;
        pool.close().await;
        Ok(())
    }
}
