SHOW INDEX FROM ride_statuses;
SHOW INDEX FROM chair_locations;
SET @profile_ride=(SELECT ride_id FROM ride_statuses GROUP BY ride_id HAVING COUNT(*) >= 6 LIMIT 1);
SET @profile_chair=(SELECT chair_id FROM chair_locations LIMIT 1);
EXPLAIN SELECT status FROM ride_statuses WHERE ride_id=@profile_ride ORDER BY created_at DESC LIMIT 1;
EXPLAIN SELECT * FROM ride_statuses WHERE ride_id=@profile_ride AND app_sent_at IS NULL ORDER BY created_at ASC LIMIT 1;
EXPLAIN SELECT * FROM ride_statuses WHERE ride_id=@profile_ride AND chair_sent_at IS NULL ORDER BY created_at ASC LIMIT 1;
EXPLAIN SELECT * FROM ride_statuses WHERE ride_id=@profile_ride ORDER BY created_at;
EXPLAIN SELECT * FROM chair_locations WHERE chair_id=@profile_chair ORDER BY created_at DESC LIMIT 1;
