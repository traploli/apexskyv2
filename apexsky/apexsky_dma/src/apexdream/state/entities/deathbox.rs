use super::*;

#[derive(Default, Debug, Clone)]
pub struct DeathboxEntity {
    pub entity_ptr: sdk::Ptr,
    pub entity_size: u32,
    pub index: u32,
    pub origin: [f32; 3],
}
impl DeathboxEntity {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(entity_ptr: sdk::Ptr, index: u32, cc: &sdk::ClientClass) -> Box<dyn Entity> {
        let entity_size = cc.ClassSize;
        Box::new(DeathboxEntity {
            entity_ptr,
            entity_size,
            index,
            ..DeathboxEntity::default()
        })
    }
}
#[async_trait]
impl Entity for DeathboxEntity {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_ref(&self) -> EntityRef<'_> {
        EntityRef::Deathbox(self)
    }
    fn is_serialized(&self) -> bool {
        false
    }
    fn get_info(&self) -> EntityInfo {
        EntityInfo {
            entity_ptr: self.entity_ptr,
            index: self.index as usize,
            handle: sdk::EHandle::from(self.index),
            rate: 128,
        }
    }
    #[instrument(skip_all)]
    async fn update(&mut self, api: &Api, ctx: &UpdateContext) {
        #[derive(sdk::FromBytes, sdk::IntoBytes)]
        #[repr(C)]
        struct Indices {
            origin_shadow_ptr: [u32; 2],
        }

        let data = &ctx.data;
        let mut indices = Indices {
            origin_shadow_ptr: [
                data.entity_origin,
                data.entity_origin + 4,
            ],
        };

        if let Ok(fields) = api
            .vm_gatherd(self.entity_ptr, self.entity_size, true, &mut indices)
            .await
        {
            let shadow_ptr = fields.origin_shadow_ptr[0] as u64
                | ((fields.origin_shadow_ptr[1] as u64) << 32);
            if let Ok(origin) = crate::apexdream::base::cr3::read_shadow_origin(
                api, shadow_ptr, api.apex_base.into_raw(),
            ).await {
                self.origin = origin;
            }
        }
    }
}
