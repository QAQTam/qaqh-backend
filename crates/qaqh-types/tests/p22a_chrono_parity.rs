//! P2-2a 反证/parity：`chrono` 换手写 `civil_from_days` 的逐字等价性。
//!
//! 本文件把退役前的 Hinnant 算法**原样复刻**进测试，对固定 epoch 秒语料断言
//! `platform::utc8_text_for`（chrono 实现）与复刻输出逐字节一致——若 chrono
//! 格式化在闰年/世纪/换日边界与旧算法有任何出入，测试即红。这是"换轮子
//! 不换 wire 文本"的机器证明，而非人工目测。

use qaqh_types::platform::utc8_text_for;

/// 退役前 `qaqh_types::platform::civil_from_days`（Hinnant 算法）逐字复刻。
fn old_civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// 退役前 `now_utc8` 的纯函数形态逐字复刻（给定 epoch 秒）。
fn old_utc8_text(epoch_secs: u64) -> String {
    let secs = epoch_secs + 8 * 3600;
    let days = secs / 86400;
    let day_secs = secs % 86400;
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let (y, m, d) = old_civil_from_days(days as i64);
    format!("UTC+8 {y:04}-{m:02}-{d:02} {hours:02}:{minutes:02}")
}

#[test]
fn chrono_utc8_matches_retired_algorithm_byte_for_byte() {
    let corpus: &[u64] = &[
        0,                    // 1970-01-01 08:00
        1,                    // 进位前夜
        3600,                 // 整点
        4 * 86400 - 1,        // 换日前一秒
        1_000_000_000,        // 2001-09
        1_700_000_000,        // 2023-11-14
        1_709_208_000,        // 2024-02-29 12:00（闰日）
        1_709_294_400,        // 2024-03-01（闰日次日）
        1_795_305_600,        // 2026-11-22（本仓既有断言锚点）
        4_107_456_000,        // 2100-03-15（非闰世纪年）
        951_782_400,          // 2000-02-29（千年闰日）
        u64::from(u32::MAX),  // 2038 溢出位（i32 边界）
    ];
    for &secs in corpus {
        assert_eq!(
            utc8_text_for(secs),
            old_utc8_text(secs),
            "epoch_secs={secs}"
        );
    }
}

#[test]
fn chrono_utc8_saturates_instead_of_inventing_now() {
    // 超 chrono 表示域（>2621-01-01）：饱和到 MAX_UTC 文本（chrono 对
    // 5 位以上年份带 `+` 号前缀），不得伪造"今天"，也不得环绕到 1970。
    let saturated = utc8_text_for(u64::MAX);
    assert!(saturated.contains("2621"), "got: {saturated}");
    assert!(!saturated.contains("1970"), "got: {saturated}");
}
