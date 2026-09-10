use log::{LevelFilter, info};
use rustwide::logging::{self, LogStorage};

#[test]
#[should_panic = "Rustwide logging is not initialized; call rustwide::logging::init() or init_with() before capture()"]
fn test_not_initialized() {
    let storage = LogStorage::new(LevelFilter::Info);
    logging::capture(&storage, || {
        info!("Hello world");
    });
}
