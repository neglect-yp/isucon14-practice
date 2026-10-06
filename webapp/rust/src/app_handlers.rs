use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum_extra::extract::CookieJar;
use ulid::Ulid;

use crate::models::{Chair, Coupon, Owner, PaymentToken, Ride, User};
use crate::{AppState, Coordinate, Error};

pub fn app_routes(app_state: AppState) -> axum::Router<AppState> {
    let routes = axum::Router::new().route("/api/app/users", axum::routing::post(app_post_users));

    let authed_routes = axum::Router::new()
        .route(
            "/api/app/payment-methods",
            axum::routing::post(app_post_payment_methods),
        )
        .route(
            "/api/app/rides",
            axum::routing::get(app_get_rides).post(app_post_rides),
        )
        .route(
            "/api/app/rides/estimated-fare",
            axum::routing::post(app_post_rides_estimated_fare),
        )
        .route(
            "/api/app/rides/:ride_id/evaluation",
            axum::routing::post(app_post_ride_evaluation),
        )
        .route(
            "/api/app/notification",
            axum::routing::get(app_get_notification),
        )
        .route(
            "/api/app/nearby-chairs",
            axum::routing::get(app_get_nearby_chairs),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            app_state.clone(),
            crate::middlewares::app_auth_middleware,
        ));

    routes.merge(authed_routes)
}

#[derive(Debug, serde::Deserialize)]
struct AppPostUsersRequest {
    username: String,
    firstname: String,
    lastname: String,
    date_of_birth: String,
    invitation_code: Option<String>,
}

#[derive(Debug, serde::Serialize)]
struct AppPostUsersResponse {
    id: String,
    invitation_code: String,
}

async fn app_post_users(
    State(AppState { pool, .. }): State<AppState>,
    jar: CookieJar,
    axum::Json(req): axum::Json<AppPostUsersRequest>,
) -> Result<(CookieJar, (StatusCode, axum::Json<AppPostUsersResponse>)), Error> {
    let user_id = Ulid::new().to_string();
    let access_token = crate::secure_random_str(32);
    let invitation_code = crate::secure_random_str(15);

    let mut tx = pool.begin().await?;

    // Serialize invitations on the inviter's existing row, before reading the
    // coupon count or inserting coupons. Locking an absent coupon code would
    // lock index gaps and can deadlock with another registration or ride.
    let inviter = if let Some(code) = req.invitation_code.filter(|code| !code.is_empty()) {
        let Some(inviter): Option<User> =
            sqlx::query_as("SELECT * FROM users WHERE invitation_code = ? FOR UPDATE")
                .bind(&code)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Err(Error::BadRequest("この招待コードは使用できません。"));
        };
        // This is the transaction's first consistent read, after the row lock.
        let invitations: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM coupons WHERE code = ?")
            .bind(format!("INV_{code}"))
            .fetch_one(&mut *tx)
            .await?;
        if invitations >= 3 {
            return Err(Error::BadRequest("この招待コードは使用できません。"));
        }
        Some((inviter, code))
    } else {
        None
    };

    sqlx::query("INSERT INTO users (id, username, firstname, lastname, date_of_birth, access_token, invitation_code) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&user_id)
        .bind(req.username)
        .bind(req.firstname)
        .bind(req.lastname)
        .bind(req.date_of_birth)
        .bind(&access_token)
        .bind(&invitation_code)
        .execute(&mut *tx)
        .await?;

    // 初回登録キャンペーンのクーポンを付与
    sqlx::query("INSERT INTO coupons (user_id, code, discount) VALUES (?, ?, ?)")
        .bind(&user_id)
        .bind("CP_NEW2024")
        .bind(3000)
        .execute(&mut *tx)
        .await?;

    // 招待コードを使った登録
    if let Some((inviter, req_invitation_code)) = inviter {
        // 招待クーポン付与
        sqlx::query("INSERT INTO coupons (user_id, code, discount) VALUES (?, ?, ?)")
            .bind(&user_id)
            .bind(format!("INV_{req_invitation_code}"))
            .bind(1500)
            .execute(&mut *tx)
            .await?;
        // 招待した人にもRewardを付与
        // The invited user's ID also makes rewards unique within one millisecond.
        sqlx::query("INSERT INTO coupons (user_id, code, discount) VALUES (?, ?, ?)")
            .bind(inviter.id)
            .bind(format!("RWD_{req_invitation_code}_{user_id}"))
            .bind(1000)
            .execute(&mut *tx)
            .await?;
    }

    tx.commit().await?;

    let jar = jar
        .add(axum_extra::extract::cookie::Cookie::build(("app_session", access_token)).path("/"));

    Ok((
        jar,
        (
            StatusCode::CREATED,
            axum::Json(AppPostUsersResponse {
                id: user_id,
                invitation_code,
            }),
        ),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostPaymentMethodsRequest {
    token: String,
}

async fn app_post_payment_methods(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    axum::Json(req): axum::Json<AppPostPaymentMethodsRequest>,
) -> Result<StatusCode, Error> {
    sqlx::query("INSERT INTO payment_tokens (user_id, token) VALUES (?, ?)")
        .bind(user.id)
        .bind(req.token)
        .execute(&pool)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, serde::Serialize)]
struct GetAppRidesResponse {
    rides: Vec<GetAppRidesResponseItem>,
}

#[derive(Debug, serde::Serialize)]
struct GetAppRidesResponseItem {
    id: String,
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
    chair: GetAppRidesResponseItemChair,
    fare: i32,
    evaluation: i32,
    requested_at: i64,
    completed_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct GetAppRidesResponseItemChair {
    id: String,
    owner: String,
    name: String,
    model: String,
}

async fn app_get_rides(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
) -> Result<axum::Json<GetAppRidesResponse>, Error> {
    let mut tx = pool.begin().await?;

    let rides: Vec<Ride> =
        sqlx::query_as("SELECT * FROM rides WHERE user_id = ? ORDER BY created_at DESC")
            .bind(&user.id)
            .fetch_all(&mut *tx)
            .await?;

    let mut items = Vec::with_capacity(rides.len());
    for ride in rides {
        let status = crate::get_latest_ride_status(&mut *tx, &ride.id).await?;
        if status != "COMPLETED" {
            continue;
        }

        let fare = calculate_discounted_fare(
            &mut tx,
            &user.id,
            Some(&ride),
            ride.pickup_latitude,
            ride.pickup_longitude,
            ride.destination_latitude,
            ride.destination_longitude,
        )
        .await?;

        let chair: Chair = sqlx::query_as("SELECT * FROM chairs WHERE id = ?")
            .bind(&ride.chair_id)
            .fetch_one(&mut *tx)
            .await?;

        let owner: Owner = sqlx::query_as("SELECT * FROM owners WHERE id = ?")
            .bind(chair.owner_id)
            .fetch_one(&mut *tx)
            .await?;

        items.push(GetAppRidesResponseItem {
            id: ride.id,
            pickup_coordinate: Coordinate {
                latitude: ride.pickup_latitude,
                longitude: ride.pickup_longitude,
            },
            destination_coordinate: Coordinate {
                latitude: ride.destination_latitude,
                longitude: ride.destination_longitude,
            },
            chair: GetAppRidesResponseItemChair {
                id: chair.id,
                owner: owner.name,
                name: chair.name,
                model: chair.model,
            },
            fare,
            evaluation: ride.evaluation.unwrap(),
            requested_at: ride.created_at.timestamp_millis(),
            completed_at: ride.updated_at.timestamp_millis(),
        });
    }

    tx.commit().await?;

    Ok(axum::Json(GetAppRidesResponse { rides: items }))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostRidesRequest {
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
}

#[derive(Debug, serde::Serialize)]
struct AppPostRidesResponse {
    ride_id: String,
    fare: i32,
}

async fn app_post_rides(
    State(AppState {
        pool,
        notifications,
        ..
    }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    axum::Json(req): axum::Json<AppPostRidesRequest>,
) -> Result<(StatusCode, axum::Json<AppPostRidesResponse>), Error> {
    let ride_id = Ulid::new().to_string();

    let mut tx = pool.begin().await?;

    let rides: Vec<Ride> = sqlx::query_as("SELECT * FROM rides WHERE user_id = ?")
        .bind(&user.id)
        .fetch_all(&mut *tx)
        .await?;

    let mut continuing_ride_count = 0;
    for ride in rides {
        let status = crate::get_latest_ride_status(&mut *tx, &ride.id).await?;
        if status != "COMPLETED" {
            continuing_ride_count += 1;
        }
    }

    if continuing_ride_count > 0 {
        return Err(Error::Conflict("ride already exists"));
    }

    sqlx::query("INSERT INTO rides (id, user_id, pickup_latitude, pickup_longitude, destination_latitude, destination_longitude) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&ride_id)
        .bind(&user.id)
        .bind(req.pickup_coordinate.latitude)
        .bind(req.pickup_coordinate.longitude)
        .bind(req.destination_coordinate.latitude)
        .bind(req.destination_coordinate.longitude)
        .execute(&mut *tx)
        .await?;

    sqlx::query("INSERT INTO ride_statuses (id, ride_id, status) VALUES (?, ?, ?)")
        .bind(Ulid::new().to_string())
        .bind(&ride_id)
        .bind("MATCHING")
        .execute(&mut *tx)
        .await?;

    let ride_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM rides WHERE user_id = ?")
        .bind(&user.id)
        .fetch_one(&mut *tx)
        .await?;

    if ride_count == 1 {
        // 初回利用で、初回利用クーポンがあれば必ず使う
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND code = 'CP_NEW2024' AND used_by IS NULL FOR UPDATE")
            .bind(&user.id)
            .fetch_optional(&mut *tx)
            .await?;
        if coupon.is_some() {
            sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = 'CP_NEW2024'")
                .bind(&ride_id)
                .bind(&user.id)
                .execute(&mut *tx)
                .await?;
        } else {
            // 無ければ他のクーポンを付与された順番に使う
            let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1 FOR UPDATE")
                .bind(&user.id)
                .fetch_optional(&mut *tx)
                .await?;
            if let Some(coupon) = coupon {
                sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = ?")
                    .bind(&ride_id)
                    .bind(&user.id)
                    .bind(coupon.code)
                    .execute(&mut *tx)
                    .await?;
            }
        }
    } else {
        // 他のクーポンを付与された順番に使う
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1 FOR UPDATE")
                .bind(&user.id)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(coupon) = coupon {
            sqlx::query("UPDATE coupons SET used_by = ? WHERE user_id = ? AND code = ?")
                .bind(&ride_id)
                .bind(&user.id)
                .bind(coupon.code)
                .execute(&mut *tx)
                .await?;
        }
    }

    let ride: Ride = sqlx::query_as("SELECT * FROM rides WHERE id = ?")
        .bind(&ride_id)
        .fetch_one(&mut *tx)
        .await?;

    let fare = calculate_discounted_fare(
        &mut tx,
        &user.id,
        Some(&ride),
        req.pickup_coordinate.latitude,
        req.pickup_coordinate.longitude,
        req.destination_coordinate.latitude,
        req.destination_coordinate.longitude,
    )
    .await?;

    tx.commit().await?;
    notifications.notify(crate::notifications::Audience::User(user.id.clone()));

    Ok((
        StatusCode::ACCEPTED,
        axum::Json(AppPostRidesResponse { ride_id, fare }),
    ))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostRidesEstimatedFareRequest {
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
}

#[derive(Debug, serde::Serialize)]
struct AppPostRidesEstimatedFareResponse {
    fare: i32,
    discount: i32,
}

async fn app_post_rides_estimated_fare(
    State(AppState { pool, .. }): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    axum::Json(req): axum::Json<AppPostRidesEstimatedFareRequest>,
) -> Result<axum::Json<AppPostRidesEstimatedFareResponse>, Error> {
    let mut tx = pool.begin().await?;

    let discounted = calculate_discounted_fare(
        &mut tx,
        &user.id,
        None,
        req.pickup_coordinate.latitude,
        req.pickup_coordinate.longitude,
        req.destination_coordinate.latitude,
        req.destination_coordinate.longitude,
    )
    .await?;

    tx.commit().await?;

    Ok(axum::Json(AppPostRidesEstimatedFareResponse {
        fare: discounted,
        discount: crate::calculate_fare(
            req.pickup_coordinate.latitude,
            req.pickup_coordinate.longitude,
            req.destination_coordinate.latitude,
            req.destination_coordinate.longitude,
        ) - discounted,
    }))
}

#[derive(Debug, serde::Deserialize)]
struct AppPostRideEvaluationRequest {
    evaluation: i32,
}

#[derive(Debug, serde::Serialize)]
struct AppPostRideEvaluationResponse {
    fare: i32,
    completed_at: i64,
}

async fn app_post_ride_evaluation(
    State(AppState {
        pool,
        notifications,
        payment_client,
        ..
    }): State<AppState>,
    Path((ride_id,)): Path<(String,)>,
    axum::Json(req): axum::Json<AppPostRideEvaluationRequest>,
) -> Result<axum::Json<AppPostRideEvaluationResponse>, Error> {
    if !(1..=5).contains(&req.evaluation) {
        return Err(Error::BadRequest("evaluation must be between 1 and 5"));
    }
    let mut tx = pool.begin().await?;
    let ride: Ride = sqlx::query_as("SELECT * FROM rides WHERE id=?")
        .bind(&ride_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::NotFound("ride not found"))?;
    if crate::get_latest_ride_status(&mut *tx, &ride_id).await? != "ARRIVED" {
        return Err(Error::BadRequest("not arrived yet"));
    }
    let token: PaymentToken = sqlx::query_as("SELECT * FROM payment_tokens WHERE user_id=?")
        .bind(&ride.user_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::BadRequest("payment token not registered"))?;
    let fare = calculate_discounted_fare(
        &mut tx,
        &ride.user_id,
        Some(&ride),
        ride.pickup_latitude,
        ride.pickup_longitude,
        ride.destination_latitude,
        ride.destination_longitude,
    )
    .await?;
    let payment_gateway_url: String =
        sqlx::query_scalar("SELECT value FROM settings WHERE name='payment_gateway_url'")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;

    // The same ride uses the same key after a timeout, retry, or process restart.
    // Hold neither a DB connection nor a row lock during the external request.
    crate::payment_gateway::request_payment_gateway_post_payment(
        &payment_client,
        &payment_gateway_url,
        &token.token,
        &ride_id,
        &crate::payment_gateway::PaymentGatewayPostPaymentRequest { amount: fare },
    )
    .await?;

    let mut tx = pool.begin().await?;
    let current: Ride = sqlx::query_as("SELECT * FROM rides WHERE id=? FOR UPDATE")
        .bind(&ride_id)
        .fetch_one(&mut *tx)
        .await?;
    let status = crate::get_latest_ride_status(&mut *tx, &ride_id).await?;
    if status == "COMPLETED" {
        tx.commit().await?;
        return Ok(axum::Json(AppPostRideEvaluationResponse {
            fare,
            completed_at: current.updated_at.timestamp_millis(),
        }));
    }
    if status != "ARRIVED" {
        return Err(Error::BadRequest("not arrived yet"));
    }
    sqlx::query("UPDATE rides SET evaluation=? WHERE id=?")
        .bind(req.evaluation)
        .bind(&ride_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO ride_statuses (id,ride_id,status) VALUES (?,?,'COMPLETED')")
        .bind(Ulid::new().to_string())
        .bind(&ride_id)
        .execute(&mut *tx)
        .await?;
    let updated_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT updated_at FROM rides WHERE id=?")
            .bind(&ride_id)
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    notifications.notify(crate::notifications::Audience::User(ride.user_id));
    if let Some(chair) = ride.chair_id {
        notifications.notify(crate::notifications::Audience::Chair(chair));
    }
    Ok(axum::Json(AppPostRideEvaluationResponse {
        fare,
        completed_at: updated_at.timestamp_millis(),
    }))
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponseData {
    ride_id: String,
    pickup_coordinate: Coordinate,
    destination_coordinate: Coordinate,
    fare: i32,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    chair: Option<AppGetNotificationResponseChair>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponseChair {
    id: String,
    name: String,
    model: String,
    stats: AppGetNotificationResponseChairStats,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNotificationResponseChairStats {
    total_rides_count: i32,
    total_evaluation_avg: f64,
}

async fn app_get_notification(
    State(state): State<AppState>,
    axum::Extension(user): axum::Extension<User>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, Error> {
    crate::notifications::respond(
        state,
        crate::notifications::Audience::User(user.id),
        headers,
    )
    .await
}

pub(crate) async fn notification_payload(
    conn: &mut sqlx::MySqlConnection,
    row: &crate::notifications::NotificationRow,
) -> Result<String, Error> {
    let ride = &row.ride;
    let fare = calculate_discounted_fare(
        conn,
        &ride.user_id,
        Some(ride),
        ride.pickup_latitude,
        ride.pickup_longitude,
        ride.destination_latitude,
        ride.destination_longitude,
    )
    .await?;
    let mut data = AppGetNotificationResponseData {
        ride_id: ride.id.clone(),
        pickup_coordinate: Coordinate {
            latitude: ride.pickup_latitude,
            longitude: ride.pickup_longitude,
        },
        destination_coordinate: Coordinate {
            latitude: ride.destination_latitude,
            longitude: ride.destination_longitude,
        },
        fare,
        status: row.status.clone(),
        chair: None,
        created_at: ride.created_at.timestamp_millis(),
        updated_at: ride.updated_at.timestamp_millis(),
    };
    if let Some(chair_id) = &ride.chair_id {
        let chair: Chair = sqlx::query_as("SELECT * FROM chairs WHERE id = ?")
            .bind(chair_id)
            .fetch_one(&mut *conn)
            .await?;
        let stats = get_chair_stats(conn, chair_id).await?;
        data.chair = Some(AppGetNotificationResponseChair {
            id: chair.id,
            name: chair.name,
            model: chair.model,
            stats,
        });
    }
    Ok(serde_json::to_string(&data)?)
}

async fn get_chair_stats(
    tx: &mut sqlx::MySqlConnection,
    chair_id: &str,
) -> Result<AppGetNotificationResponseChairStats, Error> {
    // Preserve the original eligibility checks without loading every ride and
    // issuing another query for each ride's history on every notification.
    let (count, evaluation): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), CAST(COALESCE(SUM(r.evaluation), 0) AS SIGNED)
         FROM rides r WHERE r.chair_id = ?
           AND EXISTS (SELECT 1 FROM ride_statuses s WHERE s.ride_id=r.id AND s.status='CARRYING')
           AND EXISTS (SELECT 1 FROM ride_statuses s WHERE s.ride_id=r.id AND s.status='ARRIVED')
           AND EXISTS (SELECT 1 FROM ride_statuses s WHERE s.ride_id=r.id AND s.status='COMPLETED')",
    )
    .bind(chair_id)
    .fetch_one(tx)
    .await?;
    Ok(AppGetNotificationResponseChairStats {
        total_rides_count: count as i32,
        total_evaluation_avg: if count == 0 {
            0.0
        } else {
            evaluation as f64 / count as f64
        },
    })
}

#[derive(Debug, serde::Deserialize)]
struct AppGetNearbyChairsQuery {
    latitude: i32,
    longitude: i32,
    distance: Option<i32>,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNearbyChairsResponse {
    chairs: Vec<AppGetNearbyChairsResponseChair>,
    retrieved_at: i64,
}

#[derive(Debug, serde::Serialize)]
struct AppGetNearbyChairsResponseChair {
    id: String,
    name: String,
    model: String,
    current_coordinate: Coordinate,
}

async fn nearby_chairs(
    conn: &mut sqlx::MySqlConnection,
    query: &AppGetNearbyChairsQuery,
) -> sqlx::Result<Vec<AppGetNearbyChairsResponseChair>> {
    #[derive(sqlx::FromRow)]
    struct Nearby {
        id: String,
        name: String,
        model: String,
        latitude: i32,
        longitude: i32,
    }
    // The grouping can use an index skip scan, avoiding a sort of each chair's history.
    let rows: Vec<Nearby> = sqlx::query_as(
        "SELECT c.id,c.name,c.model,l.latitude,l.longitude FROM chairs c
         JOIN (SELECT chair_id,MAX(created_at) AS latest_at FROM chair_locations GROUP BY chair_id) latest ON latest.chair_id=c.id
         JOIN chair_locations l ON l.chair_id=c.id AND l.created_at=latest.latest_at
         WHERE c.is_active=TRUE AND ABS(l.latitude-?)+ABS(l.longitude-?) <= ?
           AND l.id=(SELECT MAX(id) FROM chair_locations WHERE chair_id=c.id AND created_at=latest.latest_at)")
        .bind(query.latitude).bind(query.longitude).bind(query.distance.unwrap_or(50))
        .fetch_all(&mut *conn).await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    // Keep this as a second batched query: the optimizer otherwise checks every
    // active chair's ride history before applying the distance filter.
    let mut busy = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT DISTINCT r.chair_id FROM rides r WHERE r.chair_id IN (",
    );
    {
        let mut ids = busy.separated(",");
        for chair in &rows {
            ids.push_bind(&chair.id);
        }
    }
    busy.push(") AND COALESCE((SELECT status FROM ride_statuses s WHERE s.ride_id=r.id ORDER BY s.created_at DESC,s.id DESC LIMIT 1),'') != 'COMPLETED'");
    let busy: std::collections::HashSet<String> = busy
        .build_query_scalar()
        .fetch_all(conn)
        .await?
        .into_iter()
        .collect();
    Ok(rows
        .into_iter()
        .filter(|r| !busy.contains(&r.id))
        .map(|r| AppGetNearbyChairsResponseChair {
            id: r.id,
            name: r.name,
            model: r.model,
            current_coordinate: Coordinate {
                latitude: r.latitude,
                longitude: r.longitude,
            },
        })
        .collect())
}

async fn app_get_nearby_chairs(
    State(AppState { pool, .. }): State<AppState>,
    Query(query): Query<AppGetNearbyChairsQuery>,
) -> Result<axum::Json<AppGetNearbyChairsResponse>, Error> {
    let mut tx = pool.begin().await?;
    let chairs = nearby_chairs(&mut tx, &query).await?;
    let retrieved_at: chrono::DateTime<chrono::Utc> =
        sqlx::query_scalar("SELECT CURRENT_TIMESTAMP(6)")
            .fetch_one(&mut *tx)
            .await?;
    tx.commit().await?;
    Ok(axum::Json(AppGetNearbyChairsResponse {
        chairs,
        retrieved_at: retrieved_at.timestamp(),
    }))
}

async fn calculate_discounted_fare(
    tx: &mut sqlx::MySqlConnection,
    user_id: &str,
    ride: Option<&Ride>,
    mut pickup_latitude: i32,
    mut pickup_longitude: i32,
    mut dest_latitude: i32,
    mut dest_longitude: i32,
) -> sqlx::Result<i32> {
    let discount = if let Some(ride) = ride {
        dest_latitude = ride.destination_latitude;
        dest_longitude = ride.destination_longitude;
        pickup_latitude = ride.pickup_latitude;
        pickup_longitude = ride.pickup_longitude;

        // すでにクーポンが紐づいているならそれの割引額を参照
        let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE used_by = ?")
            .bind(&ride.id)
            .fetch_optional(&mut *tx)
            .await?;
        coupon.map(|c| c.discount).unwrap_or(0)
    } else {
        // 初回利用クーポンを最優先で使う
        let coupon: Option<Coupon> = sqlx::query_as(
            "SELECT * FROM coupons WHERE user_id = ? AND code = 'CP_NEW2024' AND used_by IS NULL",
        )
        .bind(user_id)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(coupon) = coupon {
            coupon.discount
        } else {
            // 無いなら他のクーポンを付与された順番に使う
            let coupon: Option<Coupon> = sqlx::query_as("SELECT * FROM coupons WHERE user_id = ? AND used_by IS NULL ORDER BY created_at LIMIT 1")
                .bind(user_id)
                .fetch_optional(&mut *tx)
                .await?;
            coupon.map(|c| c.discount).unwrap_or(0)
        }
    };

    let metered_fare = crate::FARE_PER_DISTANCE
        * crate::calculate_distance(
            pickup_latitude,
            pickup_longitude,
            dest_latitude,
            dest_longitude,
        );
    let discounted_metered_fare = std::cmp::max(metered_fare - discount, 0);

    Ok(crate::INITIAL_FARE + discounted_metered_fare)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing to isuride_matching_test"]
    async fn nearby_preserves_history_activity_and_latest_position() -> anyhow::Result<()> {
        let pool = sqlx::MySqlPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
        let mut tx = pool.begin().await?;
        let db: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&mut *tx)
            .await?;
        assert_eq!(db, "isuride_matching_test");
        for (id, active, x, y) in [
            ("near-free", true, 10000, 10000),
            ("near-busy", true, 10000, 10000),
            ("near-off", false, 10000, 10000),
            ("near-edge", true, 10030, 10020),
            ("near-far", true, 10051, 10000),
        ] {
            sqlx::query("INSERT INTO chairs (id,owner_id,name,model,is_active,access_token) VALUES (?,'near-owner',?,'test',?,?)").bind(id).bind(id).bind(active).bind(id).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO chair_locations (id,chair_id,latitude,longitude,created_at) VALUES (?,?,?,?,'2020-01-02')").bind(id).bind(id).bind(x).bind(y).execute(&mut *tx).await?;
        }
        sqlx::raw_sql("INSERT INTO chair_locations (id,chair_id,latitude,longitude,created_at) VALUES ('near-old','near-free',99999,99999,'2020-01-01');
          INSERT INTO rides (id,user_id,chair_id,pickup_latitude,pickup_longitude,destination_latitude,destination_longitude,created_at) VALUES
          ('near-old-busy','near-user','near-busy',0,0,0,0,'2020-01-01'),('near-new-done','near-user','near-busy',0,0,0,0,'2020-01-02'),('near-free-done','near-user','near-free',0,0,0,0,'2020-01-01');
          INSERT INTO ride_statuses (id,ride_id,status,created_at) VALUES ('near-status-old','near-old-busy','CARRYING','2020-01-01'),('near-status-new','near-new-done','COMPLETED','2020-01-02'),('near-status-free','near-free-done','COMPLETED','2020-01-01');")
          .execute(&mut *tx).await?;
        let rows = nearby_chairs(
            &mut tx,
            &AppGetNearbyChairsQuery {
                latitude: 10000,
                longitude: 10000,
                distance: None,
            },
        )
        .await?;
        let mut ids: Vec<_> = rows.iter().map(|r| r.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["near-edge", "near-free"]);
        assert_eq!(
            rows.iter()
                .find(|r| r.id == "near-free")
                .unwrap()
                .current_coordinate
                .latitude,
            10000
        );
        tx.rollback().await?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires PAYMENT_TEST_DATABASE_URL pointing to isuride_payment_test"]
    async fn payment_retry_is_idempotent_and_releases_database_connection() -> anyhow::Result<()> {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        };
        #[derive(Default)]
        struct Gateway {
            calls: AtomicUsize,
            keys: Mutex<std::collections::HashSet<String>>,
            started: tokio::sync::Notify,
            release: tokio::sync::Notify,
        }
        async fn pay(
            State(gateway): State<Arc<Gateway>>,
            headers: axum::http::HeaderMap,
        ) -> StatusCode {
            let key = headers
                .get("idempotency-key")
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            assert_eq!(key, "payment-ride");
            gateway.keys.lock().unwrap().insert(key);
            if gateway.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                gateway.started.notify_one();
                gateway.release.notified().await;
                // Simulate a successful debit whose HTTP response failed.
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::NO_CONTENT
            }
        }
        let gateway = Arc::new(Gateway::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let app = axum::Router::new()
            .route("/payments", axum::routing::post(pay))
            .with_state(gateway.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("PAYMENT_TEST_DATABASE_URL")?)
            .await?;
        let db: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&pool)
            .await?;
        assert_eq!(db, "isuride_payment_test");
        sqlx::raw_sql("INSERT INTO rides (id,user_id,chair_id,pickup_latitude,pickup_longitude,destination_latitude,destination_longitude) VALUES ('payment-ride','payment-user','payment-chair',0,0,1,1);
            INSERT INTO ride_statuses (id,ride_id,status,created_at) VALUES ('payment-carry','payment-ride','CARRYING','2020-01-01'),('payment-arrive','payment-ride','ARRIVED','2020-01-02');
            INSERT INTO payment_tokens (user_id,token) VALUES ('payment-user','test-token');")
            .execute(&pool).await?;
        sqlx::query("INSERT INTO settings (name,value) VALUES ('payment_gateway_url',?)")
            .bind(url)
            .execute(&pool)
            .await?;
        let state = AppState::new(pool.clone());
        let first_state = state.clone();
        let request = tokio::spawn(async move {
            app_post_ride_evaluation(
                State(first_state),
                Path(("payment-ride".into(),)),
                axum::Json(AppPostRideEvaluationRequest { evaluation: 5 }),
            )
            .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            gateway.started.notified(),
        )
        .await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sqlx::query("SELECT 1").execute(&pool),
        )
        .await??;
        assert_eq!(
            crate::get_latest_ride_status(&pool, "payment-ride").await?,
            "ARRIVED"
        );
        let concurrent = app_post_ride_evaluation(
            State(state),
            Path(("payment-ride".into(),)),
            axum::Json(AppPostRideEvaluationRequest { evaluation: 5 }),
        )
        .await?
        .0;
        gateway.release.notify_one();
        let response = tokio::time::timeout(std::time::Duration::from_secs(3), request)
            .await???
            .0;
        assert_eq!(response.fare, 700);
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 3);
        assert_eq!(response.completed_at, concurrent.completed_at);
        assert_eq!(gateway.keys.lock().unwrap().len(), 1);
        assert_eq!(
            crate::get_latest_ride_status(&pool, "payment-ride").await?,
            "COMPLETED"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ride_statuses WHERE status='COMPLETED'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(count, 1);
        server.abort();
        let _ = server.await;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing to isuride_matching_test"]
    async fn chair_stats_aggregate_preserves_eligibility() -> anyhow::Result<()> {
        let pool = sqlx::MySqlPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
        let mut conn = pool.acquire().await?;
        let database: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&mut *conn)
            .await?;
        assert_eq!(database, "isuride_matching_test");
        for (i, evaluation, statuses) in [
            (0, Some(5), vec!["CARRYING", "ARRIVED", "COMPLETED"]),
            (
                1,
                Some(2),
                vec!["CARRYING", "ARRIVED", "COMPLETED", "COMPLETED"],
            ),
            (2, None, vec!["CARRYING", "ARRIVED"]),
            (3, Some(1), vec!["COMPLETED"]),
        ] {
            let id = format!("stats-ride-{i}");
            sqlx::query("INSERT INTO rides (id,user_id,chair_id,pickup_latitude,pickup_longitude,destination_latitude,destination_longitude,evaluation) VALUES (?, 'stats-user', 'stats-chair', 0,0,0,0,?)")
                .bind(&id).bind(evaluation).execute(&mut *conn).await?;
            for (j, status) in statuses.iter().enumerate() {
                sqlx::query("INSERT INTO ride_statuses (id,ride_id,status) VALUES (?,?,?)")
                    .bind(format!("stats-status-{i}-{j}"))
                    .bind(&id)
                    .bind(status)
                    .execute(&mut *conn)
                    .await?;
            }
        }
        let stats = get_chair_stats(&mut conn, "stats-chair").await?;
        assert_eq!(stats.total_rides_count, 2);
        assert_eq!(stats.total_evaluation_avg, 3.5);
        let empty = get_chair_stats(&mut conn, "stats-empty").await?;
        assert_eq!(empty.total_rides_count, 0);
        assert_eq!(empty.total_evaluation_avg, 0.0);
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires TEST_DATABASE_URL pointing to isuride_matching_test"]
    async fn concurrent_invitations_respect_limit() -> anyhow::Result<()> {
        let pool = sqlx::MySqlPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
        let database: String = sqlx::query_scalar("SELECT DATABASE()")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            database, "isuride_matching_test",
            "use a disposable test database"
        );
        let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await?;
        assert_eq!(existing, 0, "test database must be empty");

        let request = |username: String, invitation_code| AppPostUsersRequest {
            username,
            firstname: "test".into(),
            lastname: "test".into(),
            date_of_birth: "2000-01-01".into(),
            invitation_code,
        };
        let state = AppState::new(pool.clone());
        let (_, (_, axum::Json(inviter))) = app_post_users(
            State(state.clone()),
            CookieJar::new(),
            axum::Json(request("inviter".into(), None)),
        )
        .await?;
        let mut registrations = tokio::task::JoinSet::new();
        for i in 0..8 {
            let state = state.clone();
            let req = request(
                format!("invitee-{i}"),
                Some(inviter.invitation_code.clone()),
            );
            registrations.spawn(async move {
                app_post_users(State(state), CookieJar::new(), axum::Json(req)).await
            });
        }
        let mut accepted = 0;
        let mut rejected = 0;
        while let Some(result) = registrations.join_next().await {
            match result? {
                Ok((_, (StatusCode::CREATED, _))) => accepted += 1,
                Err(Error::BadRequest(_)) => rejected += 1,
                other => panic!("unexpected registration result: {other:?}"),
            }
        }
        assert_eq!((accepted, rejected), (3, 5));
        let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
            .fetch_one(&pool)
            .await?;
        assert_eq!(
            users, 4,
            "rejected registrations must not leave users behind"
        );
        let coupons: Vec<(i32, i64)> = sqlx::query_as(
            "SELECT discount, COUNT(*) FROM coupons GROUP BY discount ORDER BY discount",
        )
        .fetch_all(&pool)
        .await?;
        assert_eq!(coupons, vec![(1000, 3), (1500, 3), (3000, 4)]);
        let invalid = app_post_users(
            State(state),
            CookieJar::new(),
            axum::Json(request("invalid".into(), Some("missing".into()))),
        )
        .await;
        assert!(matches!(invalid, Err(Error::BadRequest(_))));
        pool.close().await;
        Ok(())
    }
}
