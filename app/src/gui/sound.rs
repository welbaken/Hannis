//! 状态提示音:done / failed / attention 触发时播放 resource/ 下的同名 wav。
//! 用 winmm PlaySoundW(SND_FILENAME | SND_ASYNC | SND_NODEFAULT):零依赖、
//! 异步不阻塞渲染循环;文件缺失时静默(不留默认提示音)。
//! 托盘右键「提示音」可即时开关(写回 config.json)。

use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};

/// 播放 `<resource_dir>/<name>.wav`;异步返回,false = 文件缺失/播放失败。
pub fn play(resource_dir: &std::path::Path, name: &str) -> bool {
    let path = resource_dir.join(format!("{name}.wav"));
    if !path.is_file() {
        return false;
    }
    // PlaySoundW 需要以 NUL 结尾的宽字符串,且路径在其返回前必须有效:
    // 挂一块足够放任意常规路径的缓冲(SND_ASYNC 会复制文件名)。
    let wide: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(Some(0))
        .collect();
    unsafe {
        PlaySoundW(
            windows::core::PCWSTR(wide.as_ptr()),
            None,
            SND_FILENAME | SND_ASYNC | SND_NODEFAULT,
        )
        .as_bool()
    }
}
