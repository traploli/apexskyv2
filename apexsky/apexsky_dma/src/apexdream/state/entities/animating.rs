use super::*;

#[derive(Default, Debug, Clone)]
pub struct AnimatingEntity {
    pub entity_ptr: sdk::Ptr,
    pub entity_size: u32,
    pub index: u32,
    pub origin: [f32; 3],

    update_rate: u32,
    update_time: f64,

    pub model_name: ModelName,

    pub owner_entity: sdk::EHandle,

    pub skin: i32,
    pub skin_mod: i32,
    pub body: i32,
    pub camo_index: i32,

    pub spawn_time: f64,
}
impl AnimatingEntity {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(entity_ptr: sdk::Ptr, index: u32, cc: &sdk::ClientClass) -> Box<dyn Entity> {
        let entity_size = cc.ClassSize;
        Box::new(AnimatingEntity {
            entity_ptr,
            entity_size,
            index,
            ..AnimatingEntity::default()
        })
    }
}
#[async_trait]
impl Entity for AnimatingEntity {
    fn as_any(&self) -> &dyn Any {
        self
    }
    fn as_ref(&self) -> EntityRef<'_> {
        EntityRef::Animating(self)
    }
    fn is_serialized(&self) -> bool {
        false
    }
    fn get_info(&self) -> EntityInfo {
        EntityInfo {
            entity_ptr: self.entity_ptr,
            index: self.index as usize,
            handle: sdk::EHandle::from(self.index),
            rate: self.update_rate,
        }
    }
    #[instrument(skip_all)]
    async fn update(&mut self, api: &Api, ctx: &UpdateContext) {
        if self.spawn_time == 0.0 {
            self.spawn_time = ctx.time;
        }

        #[derive(sdk::FromBytes, sdk::IntoBytes)]
        #[repr(C)]
        struct Indices {
            // `entity_origin` now holds `absOriginShadowPtr` (a 64-bit pointer, split as two
            // gathered dwords), not the origin vector itself — see
            // `crate::apexdream::base::cr3` for why and how it's dereferenced.
            origin_shadow_ptr: [u32; 2],
            model_name: [u32; 2],
            owner_entity: u32,
            skin: [u32; 4],
        }

        let data = &ctx.data;
        let mut indices = Indices {
            origin_shadow_ptr: [data.entity_origin, data.entity_origin + 4],
            model_name: [data.entity_model_name, data.entity_model_name + 4],
            owner_entity: data.entity_owner_entity,
            skin: [
                data.animating_skin,
                data.animating_skin + 4,
                data.animating_skin + 8,
                data.animating_skin + 12,
            ],
        };

        if let Ok(fields) = api
            .vm_gatherd(self.entity_ptr, self.entity_size, true, &mut indices)
            .await
        {
            let origin_shadow_ptr = fields.origin_shadow_ptr[0] as u64
                | ((fields.origin_shadow_ptr[1] as u64) << 32);

            // Same primary-DTB-pointer / secondary-DTB-dereference split as `BaseEntity`: on
            // failure, `self.origin` (and `self.update_time`) are left untouched rather than
            // overwritten with a zeroed/garbage vector.
            match crate::apexdream::base::cr3::read_shadow_origin(
                api,
                origin_shadow_ptr,
                api.apex_base.into_raw(),
            )
            .await
            {
                Ok(origin) => {
                    if self.origin != origin {
                        self.update_time = ctx.time;
                    }
                    self.origin = origin;
                }
                Err(e) => tracing::trace!(?e, entity_ptr = ?self.entity_ptr, "shadow absOrigin read failed, keeping last-good"),
            }

            let model_name_ptr =
                fields.model_name[0] as u64 | ((fields.model_name[1] as u64) << 32);
            self.model_name.update(api, model_name_ptr.into()).await;

            self.owner_entity = sdk::EHandle::from(fields.owner_entity);

            // ctx.intresting
            //     .entry(self.model_name.string.clone())
            //     .or_default()
            //     .value_mut()
            //     .insert(self.index);

            self.skin = fields.skin[0] as i32;
            self.skin_mod = fields.skin[1] as i32;
            self.body = fields.skin[2] as i32;
            self.camo_index = fields.skin[3] as i32;
        }

        self.update_rate = if ctx.time >= self.update_time + 0.25 {
            512
        } else {
            4
        };
    }
}
