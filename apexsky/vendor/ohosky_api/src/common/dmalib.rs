pub const PRIO_PREEMPT: i32 = 0x10;
pub const PRIO_HIGH: i32 = 1;
pub const PRIO_LOW: i32 = 0;
pub const PRIO_PASSIVE: i32 = -1;

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct DmalibAccessTarget {
    pub target_process_name: String,
    pub override_module_base: Option<u64>,
    pub check_time_date_stamp: Option<u32>,
    pub speed_test: bool,
    pub cache_phys_addr: bool,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct FindSigResult {
    pub address: u64,
    pub data: bytes::Bytes,
}

#[allow(async_fn_in_trait)]
pub trait IMemAccess: Sized {
    fn open(target: &DmalibAccessTarget) -> anyhow::Result<Self>;

    async fn get_baseaddr(&self, priority: i32) -> anyhow::Result<Option<u64>>;

    fn get_baseaddr_blocking(&self, priority: i32) -> anyhow::Result<Option<u64>>;

    async fn read_raw(
        &self,
        addr: u64,
        len: usize,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<bytes::Bytes>;

    fn read_raw_blocking(
        &self,
        addr: u64,
        len: usize,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<bytes::Bytes>;

    async fn read_raw_into(
        &self,
        addr: u64,
        out: &mut [u8],
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()>;

    fn read_raw_into_blocking(
        &self,
        addr: u64,
        out: &mut [u8],
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()>;

    async fn read_raw_list(
        &self,
        list: &mut Vec<(u64, usize, Option<bytes::Bytes>)>,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()>;

    fn read_raw_list_blocking(
        &self,
        list: &mut Vec<(u64, usize, Option<bytes::Bytes>)>,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()>;

    #[inline]
    async fn read<T: zerocopy::FromBytes>(
        &self,
        addr: u64,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<T> {
        self.read_raw(addr, size_of::<T>(), priority, req_id)
            .await
            .map(|bytes| T::read_from_bytes(&bytes).unwrap())
    }

    async fn write_raw(
        &self,
        addr: u64,
        data: &[u8],
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()>;

    fn write_raw_blocking(
        &self,
        addr: u64,
        data: &[u8],
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()>;

    #[inline]
    async fn write<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &self,
        addr: u64,
        data: &T,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<()> {
        self.write_raw(addr, data.as_bytes(), priority, req_id)
            .await
    }

    async fn find_sig(
        &self,
        sig: &str,
        start: u64,
        end: u64,
    ) -> anyhow::Result<Option<FindSigResult>>;

    fn find_sig_blocking(
        &self,
        sig: &str,
        start: u64,
        end: u64,
    ) -> anyhow::Result<Option<FindSigResult>>;

    async fn dump(&self) -> anyhow::Result<bytes::Bytes>;

    fn dump_blocking(&self) -> anyhow::Result<bytes::Bytes>;

    /// Reads memory translated through an explicit DTB (directory table base / CR3) override
    /// instead of the connector's attached process DTB.
    ///
    /// This exists for anti-cheat mitigations that hide a field behind a pointer into a region
    /// mapped under a *different* page-table root than the game process (a "shadow" copy) —
    /// the caller must switch CR3 for exactly that one read and switch back. *Not every backend
    /// can honor a per-request DTB: the default impl here is intentionally unsupported, and only
    /// a connector that exposes something like memflow's `ProcessInstance::set_dtb` per call
    /// should override it.*
    #[allow(unused_variables)]
    async fn read_raw_with_dtb(
        &self,
        dtb: u64,
        addr: u64,
        len: usize,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<bytes::Bytes> {
        anyhow::bail!("read_raw_with_dtb: this IMemAccess backend cannot override the DTB")
    }

    #[allow(unused_variables)]
    fn read_raw_with_dtb_blocking(
        &self,
        dtb: u64,
        addr: u64,
        len: usize,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<bytes::Bytes> {
        anyhow::bail!("read_raw_with_dtb_blocking: this IMemAccess backend cannot override the DTB")
    }

    /// Cheap capability probe so callers can skip straight to a fallback path instead of
    /// paying for a round trip that is guaranteed to fail with "unsupported".
    fn supports_dtb_override(&self) -> bool {
        false
    }

    #[inline]
    async fn read_with_dtb<T: zerocopy::FromBytes>(
        &self,
        dtb: u64,
        addr: u64,
        priority: i32,
        req_id: usize,
    ) -> anyhow::Result<T> {
        self.read_raw_with_dtb(dtb, addr, size_of::<T>(), priority, req_id)
            .await
            .map(|bytes| T::read_from_bytes(&bytes).unwrap())
    }
}
