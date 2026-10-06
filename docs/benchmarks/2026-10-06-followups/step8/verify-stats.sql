SELECT COUNT(*) AS rated_chairs,
 SUM(c.chair_id IS NULL OR expected.n<>c.rides_count OR expected.s<>c.evaluation_sum) AS mismatches
FROM (
 SELECT r.chair_id,COUNT(*) AS n,COALESCE(SUM(r.evaluation),0) AS s
 FROM rides r WHERE r.chair_id IS NOT NULL
 AND EXISTS(SELECT 1 FROM ride_statuses st WHERE st.ride_id=r.id AND st.status='CARRYING')
 AND EXISTS(SELECT 1 FROM ride_statuses st WHERE st.ride_id=r.id AND st.status='ARRIVED')
 AND EXISTS(SELECT 1 FROM ride_statuses st WHERE st.ride_id=r.id AND st.status='COMPLETED')
 GROUP BY r.chair_id
) expected LEFT JOIN chair_stats c ON c.chair_id=expected.chair_id;
SELECT COUNT(*) AS cached_chairs_without_rides FROM chair_stats c
 WHERE NOT EXISTS(SELECT 1 FROM rides r WHERE r.chair_id=c.chair_id);
