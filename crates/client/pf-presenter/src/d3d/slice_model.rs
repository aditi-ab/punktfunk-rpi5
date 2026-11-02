//! The D3D11 backend's pool-slice rules, pure so the tables run on every platform: which
//! plane views a slice reuses, and the uv scale that keeps a pool's alignment rows unseen.
//! `d3d::frames` holds the views; `d3d::Presenter::present` draws with the scale.

/// Plane views per pool slice, for one pool generation at a time. A new generation drops
/// every older view, so a retired pool's texture lives no longer than the first frame of
/// the next one.
pub(crate) struct ViewCache<V> {
    generation: u64,
    views: Vec<(u32, V)>,
}

impl<V> Default for ViewCache<V> {
    fn default() -> Self {
        ViewCache {
            generation: 0,
            views: Vec::new(),
        }
    }
}

impl<V> ViewCache<V> {
    /// `slice`'s views in `generation`: the cached ones, else `make`'s. A failed `make`
    /// caches nothing.
    pub(crate) fn get_or_make<E>(
        &mut self,
        generation: u64,
        slice: u32,
        make: impl FnOnce() -> Result<V, E>,
    ) -> Result<&V, E> {
        if generation != self.generation {
            self.views.clear();
            self.generation = generation;
        }
        let i = match self.views.iter().position(|(s, _)| *s == slice) {
            Some(i) => i,
            None => {
                self.views.push((slice, make()?));
                self.views.len() - 1
            }
        };
        Ok(&self.views[i].1)
    }

    pub(crate) fn clear(&mut self) {
        self.views.clear();
    }
}

/// Display over coded size per axis: the part of a pool slice the picture fills.
pub(crate) fn uv_scale(display: (u32, u32), coded: (u32, u32)) -> [f32; 2] {
    [
        display.0 as f32 / coded.0 as f32,
        display.1 as f32 / coded.1 as f32,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_generation_drops_every_older_view() {
        // (generation, slice, the view the cache answers, views made, views held)
        let rows: &[(u64, u32, u32, u32, usize)] = &[
            (1, 0, 1, 1, 1), // first sight: made
            (1, 0, 1, 1, 1), // a seen key is reused
            (1, 3, 2, 2, 2), // another slice of the same pool: made beside it
            (1, 0, 1, 2, 2), // the first is still cached
            (2, 0, 3, 3, 1), // a new pool: every older view goes, slice 0 is made again
            (2, 3, 4, 4, 2), // and slice 3 too
        ];
        let mut cache = ViewCache::default();
        let mut made = 0;
        for &(generation, slice, want, want_made, held) in rows {
            let got = *cache
                .get_or_make(generation, slice, || {
                    made += 1;
                    Ok::<_, ()>(made)
                })
                .unwrap();
            assert_eq!(
                (got, made, cache.views.len()),
                (want, want_made, held),
                "gen {generation} slice {slice}"
            );
        }
    }

    #[test]
    fn a_failed_make_caches_nothing() {
        let mut cache = ViewCache::default();
        assert!(cache.get_or_make(1, 0, || Err(())).is_err());
        assert_eq!(cache.get_or_make(1, 0, || Ok::<_, ()>(7)), Ok(&7));
    }

    #[test]
    fn clear_drops_the_views() {
        let mut cache = ViewCache::default();
        let _ = cache.get_or_make(1, 0, || Ok::<_, ()>(1));
        cache.clear();
        assert_eq!(cache.get_or_make(1, 0, || Ok::<_, ()>(2)), Ok(&2));
    }

    #[test]
    fn uv_scale_hides_the_pool_padding() {
        let rows = [
            // 1080p in a 16-aligned pool: the last eight rows never show.
            ((1920, 1080), (1920, 1088), [1.0, 1080.0 / 1088.0]),
            // An unpadded frame samples the whole slice.
            ((2560, 1440), (2560, 1440), [1.0, 1.0]),
        ];
        for (display, coded, want) in rows {
            assert_eq!(uv_scale(display, coded), want, "{display:?} in {coded:?}");
        }
    }
}
