use axum::extract::State;
use axum::http::StatusCode;
use sqlx::{MySql, MySqlPool, QueryBuilder};

use crate::{AppState, Error};

const MATCHING_BATCH_SIZE: usize = 64;

pub fn internal_routes() -> axum::Router<AppState> {
    axum::Router::new().route(
        "/api/internal/matching",
        axum::routing::get(internal_get_matching),
    )
}

#[derive(sqlx::FromRow)]
struct WaitingRide {
    id: String,
    pickup_latitude: i32,
    pickup_longitude: i32,
}

#[derive(sqlx::FromRow)]
struct AvailableChair {
    id: String,
    latitude: i32,
    longitude: i32,
    speed: i32,
}

impl AvailableChair {
    fn pickup_distance(&self, ride: &WaitingRide) -> u64 {
        (i64::from(self.latitude) - i64::from(ride.pickup_latitude)).unsigned_abs()
            + (i64::from(self.longitude) - i64::from(ride.pickup_longitude)).unsigned_abs()
    }
}

// Rides arrive oldest first. Remove each selected chair to avoid double booking.
fn match_rides(rides: Vec<WaitingRide>, mut chairs: Vec<AvailableChair>) -> Vec<(String, String)> {
    let mut assignments = Vec::with_capacity(rides.len().min(chairs.len()));
    for ride in rides {
        let Some((index, _)) = chairs.iter().enumerate().min_by(|(_, a), (_, b)| {
            // Compare distance / speed without rounding to whole ticks.
            (a.pickup_distance(&ride) * b.speed as u64)
                .cmp(&(b.pickup_distance(&ride) * a.speed as u64))
                .then_with(|| a.id.cmp(&b.id))
        }) else {
            break;
        };
        let chair = chairs.swap_remove(index);
        assignments.push((ride.id, chair.id));
    }
    assignments
}

async fn assign_pending_rides(pool: &MySqlPool) -> sqlx::Result<usize> {
    let mut tx = pool.begin().await?;

    // Lock chairs BEFORE the first consistent read. Another matcher cannot
    // assign these chairs until commit, and the subsequent snapshot includes
    // assignments committed by earlier matchers. No process-local state is used.
    let chair_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM chairs WHERE is_active = TRUE ORDER BY id FOR UPDATE SKIP LOCKED",
    )
    .fetch_all(&mut *tx)
    .await?;
    if chair_ids.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }

    // Keep the original eligibility rule: all six statuses of EVERY previous
    // ride must have been sent to the chair before it can receive another ride.
    // Read availability and the latest positions together, rather than retrying
    // random chairs and issuing availability queries separately for every ride.
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT c.id, l.latitude, l.longitude, m.speed
         FROM chairs c
         JOIN chair_models m ON m.name = c.model
         JOIN chair_locations l ON l.id = (
             SELECT id FROM chair_locations WHERE chair_id = c.id
             ORDER BY created_at DESC, id DESC LIMIT 1
         )
         WHERE m.speed > 0 AND c.id IN (",
    );
    {
        let mut ids = query.separated(", ");
        for id in &chair_ids {
            ids.push_bind(id);
        }
    }
    query.push(
        ") AND NOT EXISTS (
            SELECT 1 FROM rides r WHERE r.chair_id = c.id
            AND (SELECT COUNT(chair_sent_at) FROM ride_statuses WHERE ride_id = r.id) <> 6
        )",
    );
    let chairs: Vec<AvailableChair> = query.build_query_as().fetch_all(&mut *tx).await?;
    if chairs.is_empty() {
        tx.commit().await?;
        return Ok(0);
    }

    let rides: Vec<WaitingRide> = sqlx::query_as(
        "SELECT id, pickup_latitude, pickup_longitude FROM rides
         WHERE chair_id IS NULL ORDER BY created_at, id LIMIT ? FOR UPDATE SKIP LOCKED",
    )
    .bind(chairs.len().min(MATCHING_BATCH_SIZE) as i64)
    .fetch_all(&mut *tx)
    .await?;
    let assignments = match_rides(rides, chairs);
    for (ride_id, chair_id) in &assignments {
        sqlx::query("UPDATE rides SET chair_id = ? WHERE id = ? AND chair_id IS NULL")
            .bind(chair_id)
            .bind(ride_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(assignments.len())
}

async fn internal_get_matching(
    State(AppState { pool, .. }): State<AppState>,
) -> Result<StatusCode, Error> {
    assign_pending_rides(&pool).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ride(id: &str, latitude: i32) -> WaitingRide {
        WaitingRide {
            id: id.into(),
            pickup_latitude: latitude,
            pickup_longitude: 0,
        }
    }

    fn chair(id: &str, latitude: i32, speed: i32) -> AvailableChair {
        AvailableChair {
            id: id.into(),
            latitude,
            longitude: 0,
            speed,
        }
    }

    #[test]
    fn matches_multiple_rides_without_reusing_chairs() {
        let result = match_rides(
            vec![ride("oldest", 0), ride("second", 100), ride("newest", 50)],
            vec![chair("near-second", 100, 1), chair("near-oldest", 0, 1)],
        );
        assert_eq!(
            result,
            vec![
                ("oldest".into(), "near-oldest".into()),
                ("second".into(), "near-second".into())
            ]
        );
    }

    #[test]
    fn uses_travel_time_not_only_distance() {
        let result = match_rides(
            vec![ride("ride", 0)],
            vec![chair("near-slow", 10, 1), chair("far-fast", 20, 4)],
        );
        assert_eq!(result, vec![("ride".into(), "far-fast".into())]);
    }

    #[test]
    fn empty_inputs_do_not_assign() {
        assert!(match_rides(vec![ride("ride", 0)], vec![]).is_empty());
        assert!(match_rides(vec![], vec![chair("chair", 0, 1)]).is_empty());
    }

    #[test]
    fn distance_does_not_overflow_i32() {
        let result = match_rides(
            vec![ride("ride", i32::MIN)],
            vec![chair("far", i32::MAX, 1), chair("near", i32::MIN, 1)],
        );
        assert_eq!(result, vec![("ride".into(), "near".into())]);
    }

    // Run only against an empty, disposable database created from 1-schema.sql.
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing to isuride_matching_test"]
    async fn database_matching_contract() -> anyhow::Result<()> {
        let pool = MySqlPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
        let database: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            database, "isuride_matching_test",
            "use a disposable test database"
        );
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chairs")
            .fetch_one(&pool)
            .await?;
        assert_eq!(existing, 0, "test database must be empty");
        sqlx::raw_sql(
            "INSERT INTO chair_models VALUES ('test', 1);
             INSERT INTO chairs (id, owner_id, name, model, is_active, access_token) VALUES
               ('free-a', 'owner', 'a', 'test', TRUE, 'a'),
               ('free-b', 'owner', 'b', 'test', TRUE, 'b'),
               ('busy', 'owner', 'busy', 'test', TRUE, 'busy'),
               ('unnotified', 'owner', 'unnotified', 'test', TRUE, 'unnotified'),
               ('inactive', 'owner', 'inactive', 'test', FALSE, 'inactive'),
               ('no-location', 'owner', 'no-location', 'test', TRUE, 'no-location');
             INSERT INTO chair_locations (id, chair_id, latitude, longitude, created_at) VALUES
               ('old-a', 'free-a', 0, 0, '2020-01-01'),
               ('new-a', 'free-a', 200, 0, '2020-01-02'),
               ('b', 'free-b', 10, 0, '2020-01-02'),
               ('busy', 'busy', 0, 0, '2020-01-02'),
               ('unnotified', 'unnotified', 0, 0, '2020-01-02'),
               ('inactive', 'inactive', 0, 0, '2020-01-02');
             INSERT INTO rides (id, user_id, chair_id, pickup_latitude, pickup_longitude,
                                destination_latitude, destination_longitude, created_at) VALUES
               ('busy-ride', 'user', 'busy', 0, 0, 0, 0, '2020-01-01'),
               ('unnotified-ride', 'user', 'unnotified', 0, 0, 0, 0, '2020-01-01'),
               ('first', 'user-1', NULL, 0, 0, 0, 0, '2020-01-02'),
               ('second', 'user-2', NULL, 100, 0, 0, 0, '2020-01-03'),
               ('third', 'user-3', NULL, 0, 0, 0, 0, '2020-01-04');",
        )
        .execute(&pool)
        .await?;
        for (i, status) in [
            "MATCHING",
            "ENROUTE",
            "PICKUP",
            "CARRYING",
            "ARRIVED",
            "COMPLETED",
        ]
        .iter()
        .enumerate()
        {
            sqlx::query("INSERT INTO ride_statuses (id, ride_id, status, chair_sent_at) VALUES (?, 'unnotified-ride', ?, ?)")
                .bind(format!("status-{i}"))
                .bind(status)
                .bind(if i < 5 { Some(chrono::Utc::now()) } else { None })
                .execute(&pool).await?;
        }

        // Concurrent requests must neither overwrite rides nor reuse a chair.
        let (a, b) = tokio::join!(assign_pending_rides(&pool), assign_pending_rides(&pool));
        assert_eq!(a? + b?, 2);
        let assigned: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, chair_id FROM rides WHERE id IN ('first', 'second', 'third') ORDER BY id",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(
            assigned,
            vec![
                ("first".into(), Some("free-b".into())),
                ("second".into(), Some("free-a".into())),
                ("third".into(), None),
            ]
        );
        assert_eq!(assign_pending_rides(&pool).await?, 0);

        // COMPLETED alone is insufficient: reuse starts only after its notification.
        sqlx::query("UPDATE ride_statuses SET chair_sent_at=NOW(6) WHERE id='status-5'")
            .execute(&pool)
            .await?;
        assert_eq!(assign_pending_rides(&pool).await?, 1);
        let third: String = sqlx::query_scalar("SELECT chair_id FROM rides WHERE id='third'")
            .fetch_one(&pool)
            .await?;
        assert_eq!(third, "unnotified");
        pool.close().await;
        Ok(())
    }
}
