use cua_driver_core::api::{
    AppId, ClientId, DispatchGuardRegistry, DispatchScope, ErrorCode, NativeProcessHandle,
    TargetKey, WindowGeneration, WindowId,
};

fn target(client: &str, app: &str, window: &str, generation: u64) -> TargetKey {
    TargetKey {
        client_id: ClientId::parse(client).unwrap(),
        app_id: AppId::parse(app).unwrap(),
        window_id: WindowId::parse(window).unwrap(),
        window_generation: WindowGeneration(generation),
    }
}

fn process(value: &str) -> NativeProcessHandle {
    NativeProcessHandle::new(value).unwrap()
}

#[test]
fn same_target_conflicts_immediately_and_drop_releases_it() {
    let registry = DispatchGuardRegistry::default();
    let scope = DispatchScope::Target {
        target: target("client", "app", "window", 1),
        process: process("process-a"),
    };
    let permit = registry.try_acquire(scope.clone()).unwrap();

    let error = registry.try_acquire(scope.clone()).unwrap_err();
    assert_eq!(error.code, ErrorCode::TargetBusy);
    assert_eq!(error.details["native_side_effect_started"], false);

    drop(permit);
    registry.try_acquire(scope).unwrap();
}

#[test]
fn different_targets_in_one_process_can_overlap_at_target_scope() {
    let registry = DispatchGuardRegistry::default();
    let _first = registry
        .try_acquire(DispatchScope::Target {
            target: target("client", "app", "window-a", 1),
            process: process("process-a"),
        })
        .unwrap();
    let _second = registry
        .try_acquire(DispatchScope::Target {
            target: target("client", "app", "window-b", 1),
            process: process("process-a"),
        })
        .unwrap();
}

#[test]
fn process_scope_excludes_every_target_in_that_process_only() {
    let registry = DispatchGuardRegistry::default();
    let _process = registry
        .try_acquire(DispatchScope::Process(process("process-a")))
        .unwrap();

    let same_process = registry
        .try_acquire(DispatchScope::Target {
            target: target("client", "app", "window-a", 1),
            process: process("process-a"),
        })
        .unwrap_err();
    assert_eq!(same_process.code, ErrorCode::TargetBusy);

    registry
        .try_acquire(DispatchScope::Process(process("process-b")))
        .unwrap();
}

#[test]
fn desktop_scope_excludes_all_native_dispatch() {
    let registry = DispatchGuardRegistry::default();
    let _desktop = registry.try_acquire(DispatchScope::Desktop).unwrap();

    for scope in [
        DispatchScope::Desktop,
        DispatchScope::Process(process("process-a")),
        DispatchScope::Target {
            target: target("client", "app", "window", 1),
            process: process("process-a"),
        },
    ] {
        assert_eq!(
            registry.try_acquire(scope).unwrap_err().code,
            ErrorCode::TargetBusy
        );
    }
}
