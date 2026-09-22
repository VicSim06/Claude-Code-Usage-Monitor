use super::*;

fn times(idle: u64, kernel: u64, user: u64) -> CpuTimes {
    CpuTimes::from_kernel_user(idle, kernel, user)
}

#[test]
fn the_first_reading_of_a_session_reports_no_cpu_load_yet() {
    let first = times(0, 0, 0);
    assert_eq!(cpu_percent_between(first, first), None);
}

#[test]
fn a_fully_idle_interval_reads_zero_percent() {
    let previous = times(1_000, 1_000, 0);
    let current = times(2_000, 2_000, 0);
    assert_eq!(cpu_percent_between(previous, current), Some(0));
}

#[test]
fn an_interval_split_between_idle_and_work_reads_half() {
    let previous = times(0, 0, 0);
    // 1000 ticks of kernel time, half of it idle.
    let current = times(500, 1_000, 0);
    assert_eq!(cpu_percent_between(previous, current), Some(50));
}

#[test]
fn user_time_counts_as_busy_on_top_of_kernel_time() {
    let previous = times(0, 0, 0);
    // 750 idle out of 1000 kernel, plus 1000 user: 1250 busy of 2000.
    let current = times(750, 1_000, 1_000);
    assert_eq!(cpu_percent_between(previous, current), Some(63));
}

#[test]
fn counters_that_go_backwards_after_a_resume_report_nothing() {
    let previous = times(5_000, 9_000, 3_000);
    let current = times(10, 20, 10);
    assert_eq!(cpu_percent_between(previous, current), None);
}

#[test]
fn a_suspended_machine_whose_counters_stood_still_reports_nothing() {
    let previous = times(1_000, 4_000, 2_000);
    let current = times(1_200, 4_000, 2_000);
    assert_eq!(cpu_percent_between(previous, current), None);
}

#[test]
fn idle_racing_ahead_of_kernel_time_is_clamped_to_fully_idle() {
    let previous = times(0, 0, 0);
    let current = times(5_000, 1_000, 0);
    assert_eq!(cpu_percent_between(previous, current), Some(0));
}

#[test]
fn memory_is_reported_in_whole_megabytes_using_windows_own_load_figure() {
    let gigabyte = 1024 * 1024 * 1024u64;
    let (percent, used_mb, total_mb) = memory_reading(32 * gigabyte, 8 * gigabyte, 74);
    assert_eq!(percent, 74);
    assert_eq!(used_mb, 24 * 1024);
    assert_eq!(total_mb, 32 * 1024);
}

#[test]
fn an_implausible_memory_load_is_clamped_to_one_hundred() {
    let (percent, _, _) = memory_reading(1024 * 1024, 0, 250);
    assert_eq!(percent, 100);
}

#[test]
fn more_available_memory_than_installed_never_underflows_the_used_figure() {
    let (_, used_mb, total_mb) = memory_reading(4 * 1024 * 1024, 8 * 1024 * 1024, 0);
    assert_eq!(used_mb, 0);
    assert_eq!(total_mb, 4);
}

#[test]
fn a_sampler_starts_with_an_idle_reading_and_a_known_processor_count() {
    let sampler = SystemSampler::new();
    let metrics = sampler.latest();
    assert_eq!(metrics.cpu_percent, 0);
    assert_eq!(metrics.memory_percent, 0);
    assert!(
        metrics.cpu_count > 0,
        "expected at least one logical processor"
    );
}
