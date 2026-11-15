//! v2.3.2 品牌资产完整性：ICO 必须包含 16/24/32/48/256 五档，
//! 256 PNG 必须是 256×256。构建脚本与运行时托盘都依赖这些文件，
//! 资产被误删 / 尺寸漏档时在 CI 阶段直接红灯。
//!
//! v2.3.2（GitHub 清理）：`assets/icon-v2.ico` 已删（历史草稿），不再检查。

use std::fs;
use std::path::Path;

fn ico_sizes(bytes: &[u8]) -> Vec<u16> {
    assert!(bytes.len() >= 6, "ICO 太短");
    assert_eq!(&bytes[0..4], &[0, 0, 1, 0], "不是 ICO 头");
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    assert!(count >= 1, "ICO 没有任何图像");
    let mut sizes = Vec::with_capacity(count);
    for i in 0..count {
        let off = 6 + i * 16;
        assert!(off + 16 <= bytes.len(), "ICO 目录被截断");
        let w = bytes[off];
        let h = bytes[off + 1];
        let w = if w == 0 { 256 } else { w as u16 };
        let h = if h == 0 { 256 } else { h as u16 };
        assert_eq!(w, h, "ICO 第 {i} 项不是正方形");
        sizes.push(w);
    }
    sizes.sort_unstable();
    sizes
}

#[test]
fn icon_assets_have_required_sizes() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let ico_path = root.join("assets").join("icon.ico");
    let ico = fs::read(&ico_path).expect("缺少 assets/icon.ico");
    assert_eq!(
        ico_sizes(&ico),
        vec![16, 24, 32, 48, 256],
        "assets/icon.ico 必须包含 16/24/32/48/256 五档"
    );

    // tauri-build 的窗口/EXE 图标路径与托盘 fallback 都要有同一份新版资源。
    let p = root.join("icons/icon.ico");
    let bytes = fs::read(&p).unwrap_or_else(|e| panic!("缺少 icons/icon.ico: {e}"));
    assert_eq!(
        ico_sizes(&bytes),
        vec![16, 24, 32, 48, 256],
        "icons/icon.ico 的尺寸档位不完整"
    );

    let png_path = root.join("assets").join("icon-v2-256.png");
    let png = fs::read(&png_path).expect("缺少 assets/icon-v2-256.png");
    assert_eq!(&png[0..8], b"\x89PNG\r\n\x1a\n", "不是 PNG");
    let w = u32::from_be_bytes([png[16], png[17], png[18], png[19]]);
    let h = u32::from_be_bytes([png[20], png[21], png[22], png[23]]);
    assert_eq!((w, h), (256, 256), "品牌 PNG 必须是 256×256");
}
