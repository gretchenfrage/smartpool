
/// General purpose imports for using the smartpool system.

pub use pool::{
    OwnedPool,
    Pool,
};
pub use channel::{
    Exec,
    ExecParam,
};
pub use timescheduler::TimeScheduler;
pub use run::{
    run,
    try_run
};
pub use scoped::{
    scoped,
    Scope,
};

pub mod setup {
    /// Imports for configuring a pool behavior.

    pub use super::*;
    pub use ::{
        PoolBehavior,
        PoolConfig,
        ChannelParams,
        ChannelToucher,
        ChannelToucherMut,
        RunningTask,
        ScheduleAlgorithm,
    };
    pub use pool::{
        OwnedPool
    };
    pub use channel::{
        VecDequeChannel,
        ShortestDeadlineFirst,
        MultiChannel,
        Exec,
        ExecParam,
        Channel
    };
    pub use scoped::{
        scoped,
        Scope,
    };
}