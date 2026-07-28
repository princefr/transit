use std::collections::HashMap;

/// Haversine distance in meters.
pub fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    const R: f64 = 6_371_000.0;
    let to_rad = |d: f64| d.to_radians();
    let dlat = to_rad(lat2 - lat1);
    let dlon = to_rad(lon2 - lon1);
    let a = (dlat / 2.0).sin().powi(2)
        + to_rad(lat1).cos() * to_rad(lat2).cos() * (dlon / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().asin();
    R * c
}

/// Approximate meters per degree of latitude (constant).
const M_PER_DEG_LAT: f64 = 111_320.0;

fn m_per_deg_lon(lat: f64) -> f64 {
    M_PER_DEG_LAT * lat.to_radians().cos().abs().max(0.2)
}

/// Uniform grid over lat/lon for O(n) neighborhood queries at national scale.
#[derive(Debug, Clone)]
pub struct GeoGrid {
    /// Cell edge length in degrees of latitude (≈ meters / 111320).
    cell_deg: f64,
    /// Bucket key (ix, iy) → indices into the original points slice.
    buckets: HashMap<(i32, i32), Vec<usize>>,
}

impl GeoGrid {
    /// Build a grid sized so that `radius_m` spans roughly one cell (or a few).
    pub fn build(points: &[(usize, f64, f64)], radius_m: f64) -> Self {
        // Cell ≈ radius so each query checks a small neighborhood (3×3 / 5×5).
        let cell_m = radius_m.max(50.0);
        let cell_deg = (cell_m / M_PER_DEG_LAT).max(1e-5);
        let mut buckets: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
        for (i, &(_id, lat, lon)) in points.iter().enumerate() {
            let key = Self::cell_key(lat, lon, cell_deg);
            buckets.entry(key).or_default().push(i);
        }
        Self { cell_deg, buckets }
    }

    fn cell_key(lat: f64, lon: f64, cell_deg: f64) -> (i32, i32) {
        let iy = (lat / cell_deg).floor() as i32;
        let ix = (lon / cell_deg).floor() as i32;
        (ix, iy)
    }

    fn neighbor_range(&self, lat: f64, lon: f64, radius_m: f64) -> (i32, i32, i32, i32) {
        let dlat = radius_m / M_PER_DEG_LAT;
        let dlon = radius_m / m_per_deg_lon(lat);
        let (ix0, iy0) = Self::cell_key(lat - dlat, lon - dlon, self.cell_deg);
        let (ix1, iy1) = Self::cell_key(lat + dlat, lon + dlon, self.cell_deg);
        (ix0.min(ix1), iy0.min(iy1), ix0.max(ix1), iy0.max(iy1))
    }

    /// Indices into `points` within `radius_m` of (lat, lon), with haversine distances.
    pub fn query(
        &self,
        points: &[(usize, f64, f64)],
        lat: f64,
        lon: f64,
        radius_m: f64,
    ) -> Vec<(usize, f64)> {
        let (ix0, iy0, ix1, iy1) = self.neighbor_range(lat, lon, radius_m);
        let mut out = Vec::new();
        for iy in iy0..=iy1 {
            for ix in ix0..=ix1 {
                if let Some(bucket) = self.buckets.get(&(ix, iy)) {
                    for &pi in bucket {
                        let (_id, pla, plo) = points[pi];
                        let d = haversine_m(lat, lon, pla, plo);
                        if d <= radius_m {
                            out.push((pi, d));
                        }
                    }
                }
            }
        }
        out
    }
}

/// Hub linking for station points within radius using a grid spatial index.
/// Returns pairs of original point ids (not slice indices) and distance meters.
pub fn hub_links(points: &[(usize, f64, f64)], radius_m: f64) -> Vec<(usize, usize, f64)> {
    if points.len() < 2 {
        return Vec::new();
    }
    let grid = GeoGrid::build(points, radius_m);
    let mut out = Vec::new();
    let mut seen: HashMap<(usize, usize), ()> = HashMap::new();

    for (i, &(ia, la, loa)) in points.iter().enumerate() {
        let hits = grid.query(points, la, loa, radius_m);
        for (j, d) in hits {
            if j <= i {
                continue;
            }
            if d <= 1.0 {
                continue;
            }
            let ib = points[j].0;
            let key = if ia < ib { (ia, ib) } else { (ib, ia) };
            if seen.insert(key, ()).is_some() {
                continue;
            }
            out.push((key.0, key.1, d));
        }
    }
    out
}

/// Find stop indices (original ids from points) within max_m of a point, nearest first.
pub fn nearby(
    points: &[(usize, f64, f64)],
    lat: f64,
    lon: f64,
    max_m: f64,
    limit: usize,
) -> Vec<(usize, f64)> {
    if points.is_empty() || limit == 0 {
        return Vec::new();
    }
    let grid = GeoGrid::build(points, max_m);
    let mut scored: Vec<(usize, f64)> = grid
        .query(points, lat, lon, max_m)
        .into_iter()
        .map(|(pi, d)| (points[pi].0, d))
        .collect();
    scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paris_lyon_distance() {
        let d = haversine_m(48.85, 2.35, 45.76, 4.86);
        assert!(d > 300_000.0 && d < 500_000.0);
    }

    #[test]
    fn nearby_finds_close_only() {
        let points = vec![
            (0, 48.8600, 2.3500),
            (1, 48.8605, 2.3505), // ~60m
            (2, 45.7600, 4.8600), // Lyon — far
        ];
        let hits = nearby(&points, 48.86, 2.35, 200.0, 10);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, 0);
        assert!(hits.iter().all(|(id, _)| *id != 2));
    }

    #[test]
    fn hub_links_grid_matches_pairs_in_radius() {
        // Three stations: A near B, C far from both
        let points = vec![
            (10, 48.8600, 2.3500),
            (20, 48.8602, 2.3502), // ~25m from A
            (30, 45.7600, 4.8600),
        ];
        let links = hub_links(&points, 100.0);
        assert_eq!(links.len(), 1);
        let (a, b, d) = links[0];
        assert!((a == 10 && b == 20) || (a == 20 && b == 10));
        assert!(d > 1.0 && d < 100.0);
    }

    #[test]
    fn hub_links_empty_and_singleton() {
        assert!(hub_links(&[], 100.0).is_empty());
        assert!(hub_links(&[(1, 0.0, 0.0)], 100.0).is_empty());
    }

    #[test]
    fn grid_handles_many_points() {
        // Synthetic grid of stations ~500m apart; radius 600m should link neighbors
        let mut points = Vec::new();
        for i in 0..20 {
            for j in 0..20 {
                let lat = 48.0 + (i as f64) * 0.0045; // ~500m
                let lon = 2.0 + (j as f64) * 0.0065;
                points.push((i * 20 + j, lat, lon));
            }
        }
        let links = hub_links(&points, 600.0);
        assert!(!links.is_empty());
        // No self-links; distances within radius
        for (a, b, d) in &links {
            assert_ne!(a, b);
            assert!(*d <= 600.0 && *d > 1.0);
        }
    }
}
