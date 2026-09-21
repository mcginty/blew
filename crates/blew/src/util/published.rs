//! What the Linux peripheral has published, and which power-on it belongs to.
//!
//! Lives outside `platform::linux` so it can be tested on every host, and is
//! generic over the handle types so the tests need no bluer session. In the
//! backend `A` is bluer's `AdvertisementHandle` and `P` its `ApplicationHandle`;
//! dropping either unregisters what it names.
//!
//! The adapter takes the advertisement and GATT application down with it when
//! it powers off, but `start_advertising` refuses while an advertisement handle
//! is held, so a handle kept across the cycle refuses every later start (#46).
//! The generation lives beside the handles so that a start, which awaits BlueZ
//! between publishing and storing, can't store a handle after a power-off.

/// The published handles and the power generation they belong to.
///
/// Every transition is one `&mut self` method, so a caller holding this under
/// one lock acquisition can't observe it half-done. That is the point of
/// [`Published::power_lost`] taking the handles as well as retiring the
/// generation: split across two acquisitions, a start could capture the *new*
/// generation between them, store its GATT application, and have the second
/// half remove it -- leaving an advertisement up with no services behind it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct Published<A, P> {
    adv: Option<A>,
    app: Option<P>,
    power_generation: u64,
}

impl<A, P> Default for Published<A, P> {
    fn default() -> Self {
        Self {
            adv: None,
            app: None,
            power_generation: 0,
        }
    }
}

/// Handles taken out of [`Published`], to be dropped once its lock is released.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) type Taken<A, P> = (Option<A>, Option<P>);

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
impl<A, P> Published<A, P> {
    /// The generation a start stores against, or `None` while an
    /// advertisement is held.
    pub(crate) fn begin_start(&self) -> Option<u64> {
        self.adv.is_none().then_some(self.power_generation)
    }

    /// Keep the GATT application a start published, unless the adapter went
    /// down since it began. A refused handle is handed back so the caller can
    /// drop it -- unregistering it -- outside the lock.
    pub(crate) fn store_app(&mut self, generation: u64, app: P) -> Result<(), P> {
        if generation != self.power_generation {
            return Err(app);
        }
        self.app = Some(app);
        Ok(())
    }

    /// Keep the advertisement a start published; see [`Self::store_app`].
    pub(crate) fn store_adv(&mut self, generation: u64, adv: A) -> Result<(), A> {
        if generation != self.power_generation {
            return Err(adv);
        }
        self.adv = Some(adv);
        Ok(())
    }

    /// Take both handles, for `stop_advertising`.
    pub(crate) fn unpublish(&mut self) -> Taken<A, P> {
        (self.adv.take(), self.app.take())
    }

    /// The adapter powered off: retire the generation, failing any start still
    /// waiting on BlueZ, and take both handles -- in one step; see the type.
    pub(crate) fn power_lost(&mut self) -> Taken<A, P> {
        self.power_generation += 1;
        self.unpublish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slot = Published<&'static str, &'static str>;

    #[test]
    fn a_start_publishes_both_handles_and_then_refuses_another() {
        let mut slot = Slot::default();
        let generation = slot.begin_start().expect("idle");

        assert_eq!(slot.store_app(generation, "app"), Ok(()));
        assert_eq!(slot.store_adv(generation, "adv"), Ok(()));

        assert_eq!(slot.begin_start(), None);
        assert_eq!(slot.unpublish(), (Some("adv"), Some("app")));
        assert!(slot.begin_start().is_some());
    }

    /// The #46 wedge: a power-off leaves nothing that refuses the next start.
    #[test]
    fn a_power_off_takes_both_handles_and_frees_the_slot() {
        let mut slot = Slot::default();
        let generation = slot.begin_start().expect("idle");
        slot.store_app(generation, "app").unwrap();
        slot.store_adv(generation, "adv").unwrap();

        assert_eq!(slot.power_lost(), (Some("adv"), Some("app")));
        assert_eq!(slot.begin_start(), Some(generation + 1));
    }

    /// A start that began before the power-off stores nothing after it,
    /// whichever handle it had got to.
    #[test]
    fn a_start_straddling_a_power_off_hands_back_what_it_published() {
        let mut slot = Slot::default();
        let generation = slot.begin_start().expect("idle");
        slot.store_app(generation, "app").unwrap();

        assert_eq!(slot.power_lost(), (None, Some("app")));

        assert_eq!(slot.store_adv(generation, "adv"), Err("adv"));
        assert_eq!(slot.unpublish(), (None, None));

        let mut slot = Slot::default();
        let generation = slot.begin_start().expect("idle");
        slot.power_lost();
        assert_eq!(slot.store_app(generation, "app"), Err("app"));
    }

    /// The review finding on #48. With the bump and the take split, a start
    /// that captured the new generation between them stored its application
    /// and then lost it to the take, while its advertisement still went up.
    /// As one step there is no "between": a start that begins after the
    /// power-off keeps everything it publishes.
    #[test]
    fn a_start_after_a_power_off_keeps_everything_it_publishes() {
        let mut slot = Slot::default();
        let before = slot.begin_start().expect("idle");
        slot.store_app(before, "old app").unwrap();

        slot.power_lost();

        let after = slot.begin_start().expect("idle after power-off");
        assert_eq!(slot.store_app(after, "app"), Ok(()));
        assert_eq!(slot.store_adv(after, "adv"), Ok(()));
        assert_eq!(slot.unpublish(), (Some("adv"), Some("app")));
    }
}
