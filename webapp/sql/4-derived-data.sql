-- Rebuild derived totals from the complete persisted coordinate history.
INSERT INTO chair_distances (chair_id,latitude,longitude,total_distance,updated_at)
SELECT chair_id,latitude,longitude,total_distance,created_at FROM (
  SELECT chair_id,latitude,longitude,created_at,
    SUM(COALESCE(distance,0)) OVER (PARTITION BY chair_id) AS total_distance,
    ROW_NUMBER() OVER (PARTITION BY chair_id ORDER BY created_at DESC,id DESC) AS position
  FROM (
    SELECT id,chair_id,latitude,longitude,created_at,
      ABS(latitude-LAG(latitude) OVER (PARTITION BY chair_id ORDER BY created_at,id))+
      ABS(longitude-LAG(longitude) OVER (PARTITION BY chair_id ORDER BY created_at,id)) AS distance
    FROM chair_locations
  ) steps
) totals WHERE position=1
ON DUPLICATE KEY UPDATE latitude=VALUES(latitude),longitude=VALUES(longitude),
  total_distance=VALUES(total_distance),updated_at=VALUES(updated_at);
