use crate::config::Store;
use anyhow::{Context, Result};
use std::{ffi::OsString, sync::OnceLock, time::Duration};
use tokio_util::sync::CancellationToken;
use windows_service::{
    define_windows_service,
    service::*,
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

const NAME: &str = "WorkLink";
static STORE: OnceLock<Store> = OnceLock::new();
define_windows_service!(ffi_main, entry);

pub fn dispatch(store: Store) -> Result<()> {
    STORE
        .set(store)
        .map_err(|_| anyhow::anyhow!("服务重复初始化"))?;
    service_dispatcher::start(NAME, ffi_main)?;
    Ok(())
}

fn status(state: ServiceState, code: u32) -> ServiceStatus {
    ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: if state == ServiceState::Running {
            ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
        } else {
            ServiceControlAccept::empty()
        },
        exit_code: ServiceExitCode::Win32(code),
        checkpoint: if state == ServiceState::StartPending {
            1
        } else {
            0
        },
        wait_hint: Duration::from_secs(15),
        process_id: None,
    }
}

fn entry(_args: Vec<OsString>) {
    let stop = CancellationToken::new();
    let cancel = stop.clone();
    let handler = service_control_handler::register(NAME, move |control| match control {
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        ServiceControl::Stop | ServiceControl::Shutdown => {
            cancel.cancel();
            ServiceControlHandlerResult::NoError
        }
        _ => ServiceControlHandlerResult::NotImplemented,
    });
    if let Ok(handler) = handler {
        let _ = handler.set_service_status(status(ServiceState::StartPending, 0));
        let result = (|| -> Result<()> {
            let store = STORE.get().context("缺少服务配置")?.clone();
            let _logging = crate::logging(&store)?;
            store.read()?;
            let runtime = crate::runtime()?;
            handler.set_service_status(status(ServiceState::Running, 0))?;
            runtime.block_on(crate::daemon::run(store, stop))
        })();
        if let Err(error) = &result {
            tracing::error!(%error, "service stopped with error");
        }
        let _ = handler.set_service_status(status(
            ServiceState::Stopped,
            if result.is_ok() { 0 } else { 1 },
        ));
    }
}

pub fn install(store: &Store) -> Result<()> {
    store.read()?;
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("安装服务需要管理员权限，请在管理员终端运行")?;
    let info = ServiceInfo {
        name: NAME.into(),
        display_name: "WorkLink 加密远程连接".into(),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec![
            "--data-dir".into(),
            store.root.clone().into_os_string(),
            "service".into(),
            "run".into(),
        ],
        dependencies: vec![],
        account_name: None,
        account_password: None,
    };
    let service =
        manager.create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)?;
    service.set_description("通过双向认证加密连接提供 SSH 转发和明确授权的远程管理能力。")?;
    service.update_failure_actions(ServiceFailureActions {
        reset_period: ServiceFailureResetPeriod::After(Duration::from_secs(86400)),
        reboot_msg: None,
        command: None,
        actions: Some(vec![
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(5),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(15),
            },
            ServiceAction {
                action_type: ServiceActionType::Restart,
                delay: Duration::from_secs(30),
            },
        ]),
    })?;
    service.set_failure_actions_on_non_crash_failures(true)?;
    service.start::<&str>(&[])?;
    Ok(())
}

pub fn control(action: &str) -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let access = match action {
        "start" => ServiceAccess::START,
        "stop" => ServiceAccess::STOP,
        "uninstall" => ServiceAccess::STOP | ServiceAccess::DELETE,
        _ => ServiceAccess::QUERY_STATUS,
    };
    let service = manager
        .open_service(NAME, access)
        .context("服务操作失败：请确认已安装服务，并以管理员身份运行")?;
    match action {
        "start" => {
            service.start::<&str>(&[])?;
        }
        "stop" => {
            service.stop()?;
        }
        "uninstall" => {
            let _ = service.stop();
            service.delete()?;
        }
        _ => {}
    }
    Ok(())
}

pub fn query() -> Result<serde_json::Value> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service = manager.open_service(NAME, ServiceAccess::QUERY_STATUS)?;
    let state = service.query_status()?;
    Ok(
        serde_json::json!({"name":NAME,"state":format!("{:?}",state.current_state),"pid":state.process_id}),
    )
}
