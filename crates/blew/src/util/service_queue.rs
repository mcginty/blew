//! The Linux peripheral's queue of services for the next advertisement.
//!
//! Lives outside `platform::linux` so it can be tested on every host rather
//! than only against a running bluetoothd. Linux's `add_service` never reaches
//! BlueZ: it queues the service, and `start_advertising` serves the whole queue
//! as one GATT application. Nothing removes a queued service (#34), so an
//! application that re-adds its services -- which it has to after an adapter
//! power cycle on Android, and is told to do cross-platform -- would serve
//! each of them twice.

use crate::gatt::service::GattService;

/// Queue `service` for the next advertisement.
///
/// A service whose UUID is already queued is replaced in place, keeping its
/// position, so adding the same service again converges instead of serving a
/// second copy. Replacing rather than ignoring means the latest definition
/// wins, which is what a caller re-adding a changed service expects.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn queue_service(pending: &mut Vec<GattService>, service: GattService) {
    match pending
        .iter_mut()
        .find(|queued| queued.uuid == service.uuid)
    {
        Some(queued) => *queued = service,
        None => pending.push(service),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gatt::props::{AttributePermissions, CharacteristicProperties};
    use crate::gatt::service::GattCharacteristic;
    use uuid::Uuid;

    fn service(uuid: u128, chars: &[u128]) -> GattService {
        GattService {
            uuid: Uuid::from_u128(uuid),
            primary: true,
            characteristics: chars
                .iter()
                .map(|&c| GattCharacteristic {
                    uuid: Uuid::from_u128(c),
                    properties: CharacteristicProperties::READ,
                    permissions: AttributePermissions::READ,
                    value: vec![],
                    descriptors: vec![],
                })
                .collect(),
        }
    }

    fn uuids(pending: &[GattService]) -> Vec<Uuid> {
        pending.iter().map(|s| s.uuid).collect()
    }

    #[test]
    fn distinct_services_queue_in_order() {
        let mut pending = Vec::new();
        queue_service(&mut pending, service(1, &[10]));
        queue_service(&mut pending, service(2, &[20]));

        assert_eq!(uuids(&pending), [Uuid::from_u128(1), Uuid::from_u128(2)]);
    }

    /// The reported bug: re-adding after a power cycle served every service twice.
    #[test]
    fn re_adding_every_service_converges() {
        let mut pending = Vec::new();
        for _cycle in 0..3 {
            queue_service(&mut pending, service(1, &[10]));
            queue_service(&mut pending, service(2, &[20]));
        }

        assert_eq!(uuids(&pending), [Uuid::from_u128(1), Uuid::from_u128(2)]);
    }

    #[test]
    fn re_adding_one_service_keeps_its_place_and_takes_the_new_definition() {
        let mut pending = Vec::new();
        queue_service(&mut pending, service(1, &[10]));
        queue_service(&mut pending, service(2, &[20]));
        queue_service(&mut pending, service(3, &[30]));

        queue_service(&mut pending, service(2, &[21, 22]));

        assert_eq!(
            uuids(&pending),
            [Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3)]
        );
        let replaced: Vec<Uuid> = pending[1].characteristics.iter().map(|c| c.uuid).collect();
        assert_eq!(replaced, [Uuid::from_u128(21), Uuid::from_u128(22)]);
    }
}
