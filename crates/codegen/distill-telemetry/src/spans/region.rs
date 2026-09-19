//! Scope-owned measurement regions.
//!
//! A [`Region`] is a held (never entered) span whose parentage is a required constructor choice and whose close point belongs to scope.
//! It replaces raw `tracing::Span` locals with hand-placed drops.
//! Background tasks go through [`instrument_task!`], which accepts a named child or root, never a bare current span.
//! A caller's span lifetime therefore cannot be extended silently.

/// Where a region attaches in the trace tree. The choice is mandatory.
pub enum Parent<'a> {
    /// Inherit whatever span is entered at creation.
    Inherit,
    /// Attach under an explicit handle, for regions crossing tasks or threads.
    Explicit(&'a tracing::Span),
    /// Start a new tree deliberately.
    Root,
}

/// A measured region: opens at construction, closes when it goes out of scope or at an explicit [`Region::close`].
/// Held, not entered, so it is safe to keep across `.await`.
#[must_use = "dropping a Region immediately closes its span as a zero-length frame"]
pub struct Region(tracing::Span);

impl Region {
    /// Wrap a prebuilt span (the escape hatch for spans carrying fields).
    pub fn from_span(span: tracing::Span) -> Self {
        Self(span)
    }

    /// End the region here instead of at scope end.
    pub fn close(self) {}

    /// Handle for recording fields or parenting children.
    pub fn span(&self) -> &tracing::Span {
        &self.0
    }
}

/// Open a [`Region`] with an explicit [`Parent`] choice.
/// `region!("name", Parent::Inherit)` or `region!(debug, "name", ...)`.
#[macro_export]
macro_rules! region {
    ($name:literal, $parent:expr) => {
        $crate::region::Region::from_span(match $parent {
            $crate::region::Parent::Inherit => tracing::info_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::info_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::info_span!(parent: None, $name),
        })
    };
    (debug, $name:literal, $parent:expr) => {
        $crate::region::Region::from_span(match $parent {
            $crate::region::Parent::Inherit => tracing::debug_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::debug_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::debug_span!(parent: None, $name),
        })
    };
}

/// Instrument a future for a spawned task under a named child or root span.
/// There is no variant taking the bare current span: cloning it into a task holds the caller's span open until the task finishes.
#[macro_export]
macro_rules! instrument_task {
    ($name:literal, $parent:expr, $fut:expr) => {
        tracing::Instrument::instrument($fut, match $parent {
            $crate::region::Parent::Inherit => tracing::info_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::info_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::info_span!(parent: None, $name),
        })
    };
    (debug, $name:literal, $parent:expr, $fut:expr) => {
        tracing::Instrument::instrument($fut, match $parent {
            $crate::region::Parent::Inherit => tracing::debug_span!($name),
            $crate::region::Parent::Explicit(span) => tracing::debug_span!(parent: span, $name),
            $crate::region::Parent::Root => tracing::debug_span!(parent: None, $name),
        })
    };
}

#[macro_export]
macro_rules! event_span {
    ($($args:tt)+) => {
        tracing::info_span!($($args)+).in_scope(|| {})
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span_profile::test_support::folded_with_layer;

    /// Every guard pattern folds where its parent choice says, verified through the real profile layer.
    #[test]
    fn regions_fold_under_their_declared_parents() {
        let folded = folded_with_layer(|| {
            let outer = region!("outer", Parent::Inherit);
            let inner = region!("inner", Parent::Explicit(outer.span()));
            std::thread::sleep(std::time::Duration::from_millis(2));
            inner.close();
            let lone = region!("lone", Parent::Root);
            std::thread::sleep(std::time::Duration::from_millis(2));
            lone.close();
            let first = region!("first_wait", Parent::Explicit(outer.span()));
            std::thread::sleep(std::time::Duration::from_millis(2));
            first.close();
            outer.close();
        });
        let paths: Vec<&str> = folded
            .lines()
            .filter_map(|l| l.rsplit_once(' ').map(|(p, _)| p))
            .collect();
        assert!(paths.contains(&"outer"), "{folded}");
        assert!(paths.contains(&"outer;inner"), "{folded}");
        assert!(paths.contains(&"outer;first_wait"), "{folded}");
        assert!(paths.contains(&"lone"), "{folded}");
        for p in &paths {
            assert!(
                !matches!(*p, "inner" | "first_wait"),
                "child folded as root: {p}"
            );
            let segs: Vec<&str> = p.split(';').collect();
            assert!(
                segs.windows(2).all(|w| match w {
                    [a, b] => a != b,
                    _ => true,
                }),
                "duplicate frame: {p}"
            );
        }
    }
}
