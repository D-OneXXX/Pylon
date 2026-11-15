//! Windows Job Object：每个 /api/run 单独建一个 Job，子进程一进去，
//! 超时 / 退出 / Pylon 退出时，系统把 Job 里残留的进程一锅端。
//! 根治 v0.5 之前的问题：只杀直接子进程，孙进程残留、文件被锁（os error 5）。
//!
//! 注意：Job 是按“次”建的，不是全局一个——
//! 全局一个 Job 的话，一次超时的清理会把其他并发命令的孩子也杀了。
//! 这里每个 run_operation 建一个自己的 Job，守卫活到本次执行结束。
//! （本文件只在 Windows 下编译，见 main.rs 的 `#[cfg(windows)] mod job;`）

use std::os::windows::io::AsRawHandle;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// 一个 Job 的守卫：Drop 时关句柄；句柄一关，Job 里残留的进程全被系统杀掉。
/// 进程退出时句柄本来就会被系统关掉，所以 Pylon 被关掉时所有 Job 里的进程也一并死。
pub struct JobGuard(pub HANDLE);

// v2.2.0：JobGuard 要存进跨线程共享的 TaskRecord（Mutex<Vec<TaskRecord>>）。
// HANDLE 本质是内核句柄表索引，跨线程传递 / 关闭安全，这里显式声明 Send。
unsafe impl Send for JobGuard {}

impl JobGuard {
    /// 建一个带 KILL_ON_JOB_CLOSE 的 Job。失败返回 None（调用方照常跑，只是不再兜底清理）。
    pub fn new() -> Option<Self> {
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return None;
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let ok = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const std::ffi::c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if ok == 0 {
                CloseHandle(job);
                return None;
            }
            Some(JobGuard(job))
        }
    }

    /// 把一个已 spawn 的子进程放进 Job。返回 false 表示放不进去
    ///（比如它已经在别的 Job 里——不致命，调用方照常跑）。
    pub fn assign(&self, child: &std::process::Child) -> bool {
        unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as HANDLE) != 0 }
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_guard_creates() {
        // 只验证 Job 能建出来、能设 KILL_ON_JOB_CLOSE。
        // 不把当前进程放进去（放进去再关句柄会把测试进程自己杀了）。
        let guard = JobGuard::new();
        assert!(guard.is_some(), "CreateJobObjectW 失败");
    }
}
