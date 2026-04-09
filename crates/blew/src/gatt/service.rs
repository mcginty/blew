use super::props::{AttributePermissions, CharacteristicProperties};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct GattDescriptor {
    pub uuid: Uuid,
    pub value: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct GattCharacteristic {
    pub uuid: Uuid,
    pub properties: CharacteristicProperties,
    pub permissions: AttributePermissions,
    /// Static value for server-side characteristics; empty for client-discovered ones.
    pub value: Vec<u8>,
    pub descriptors: Vec<GattDescriptor>,
}

#[derive(Debug, Clone)]
pub struct GattService {
    pub uuid: Uuid,
    pub primary: bool,
    pub characteristics: Vec<GattCharacteristic>,
}
