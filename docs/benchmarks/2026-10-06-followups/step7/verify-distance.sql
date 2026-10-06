SELECT COUNT(*) AS history_chairs,
 SUM(d.chair_id IS NULL OR h.total_distance<>d.total_distance OR h.updated_at<>d.updated_at) AS mismatches
FROM (
 SELECT chair_id,SUM(COALESCE(distance,0)) AS total_distance,MAX(created_at) AS updated_at
 FROM (
  SELECT chair_id,created_at,
   ABS(latitude-LAG(latitude) OVER(PARTITION BY chair_id ORDER BY created_at,id))+
   ABS(longitude-LAG(longitude) OVER(PARTITION BY chair_id ORDER BY created_at,id)) AS distance
  FROM chair_locations
 ) steps GROUP BY chair_id
) h LEFT JOIN chair_distances d ON d.chair_id=h.chair_id;
