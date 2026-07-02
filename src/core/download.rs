//! 结果落盘: 把 Job.outputs 统一落到本地, 不论来源是远程 URL、内联字节, 还是已在本地。
//!
//! 文件名模板: `{job_id}_{index}.{ext}`。ext 推断:
//! - URL 来源: 从 URL 后缀推断, 推断不出按素材类型给默认值;
//! - 内联字节来源: 从 mime 推断(image/png -> png 等);
//! - 本地路径来源: 沿用原文件扩展名。

use std::path::{Path, PathBuf};

use crate::core::provider::{Asset, AssetKind, AssetSource, Job};

/// 根据 job_id、序号、URL 推断出本地文件名(不含目录)。
///
/// 纯函数, 无 IO, 便于单测。ext 推断规则:
/// 1. 取 URL 路径最后一段的扩展名(去掉 query/fragment)。
/// 2. 推断不出时按素材类型给默认: image->png, video->mp4, audio->mp3。
pub fn build_filename(job_id: &str, index: usize, url: &str, kind: AssetKind) -> String {
    let ext = infer_ext(url, kind);
    format!("{}_{}.{}", job_id, index, ext)
}

/// 从 URL 推断扩展名。
fn infer_ext(url: &str, kind: AssetKind) -> String {
    // 先砍掉 query 和 fragment, 只看路径部分
    let mut path_part = url;
    if let Some(pos) = path_part.find('?') {
        path_part = &path_part[..pos];
    }
    if let Some(pos) = path_part.find('#') {
        path_part = &path_part[..pos];
    }

    // 取最后一段
    let last_seg = path_part.rsplit('/').next().unwrap_or("");
    // 找扩展名: 最后一个 '.' 之后的部分, 且该部分非空、不含路径分隔
    if let Some(dot) = last_seg.rfind('.') {
        let candidate = &last_seg[dot + 1..];
        // 合理的扩展名应较短且为字母数字, 过滤掉类似版本号那种异常
        let looks_like_ext = !candidate.is_empty()
            && candidate.len() <= 5
            && candidate.chars().all(|c| c.is_ascii_alphanumeric());
        if looks_like_ext {
            return candidate.to_ascii_lowercase();
        }
    }

    // 推断不出, 按素材类型给默认扩展名
    default_ext_for_kind(kind)
}

/// 素材类型的默认扩展名。
fn default_ext_for_kind(kind: AssetKind) -> String {
    match kind {
        AssetKind::Image => "png".to_string(),
        AssetKind::Video => "mp4".to_string(),
        AssetKind::Audio => "mp3".to_string(),
    }
}

/// 由 MIME 类型推断扩展名(内联字节产物落盘用)。
/// 推断不出时按素材类型给默认。纯函数, 便于单测。
pub fn ext_from_mime(mime: &str, kind: AssetKind) -> String {
    // 只看 "type/subtype" 的 subtype, 去掉可能的参数(如 "image/png; charset")
    let main = mime.split(';').next().unwrap_or(mime).trim().to_ascii_lowercase();
    match main.as_str() {
        "image/png" => "png".to_string(),
        "image/jpeg" | "image/jpg" => "jpg".to_string(),
        "image/webp" => "webp".to_string(),
        "image/gif" => "gif".to_string(),
        "video/mp4" => "mp4".to_string(),
        "audio/mpeg" | "audio/mp3" => "mp3".to_string(),
        "audio/wav" | "audio/x-wav" => "wav".to_string(),
        // 未知 mime 回退到素材类型默认
        _ => default_ext_for_kind(kind),
    }
}

/// 组装 `{job_id}_{index}.{ext}` 文件名(ext 已知时直接用, 不再从 URL 推断)。
fn build_filename_with_ext(job_id: &str, index: usize, ext: &str) -> String {
    format!("{}_{}.{}", job_id, index, ext)
}

/// 取本地路径的扩展名; 没有则按素材类型给默认。
fn ext_from_path(path: &Path, kind: AssetKind) -> String {
    match path.extension().and_then(|e| e.to_str()) {
        Some(e) if !e.is_empty() => e.to_ascii_lowercase(),
        _ => default_ext_for_kind(kind),
    }
}

/// 把单个素材落到 out_dir, 返回落盘后的本地路径。
///
/// 按来源统一处理三种素材(D-008 把 Asset 扩展为三来源后的落盘汇聚点):
/// - URL: 发 GET 下载;
/// - 内联字节(InlineBytes): 直接写已解码字节, 无需网络;
/// - 本地路径(LocalPath): 已在本地, 复制进 out_dir(若已在 out_dir 内则原样返回)。
pub async fn download_asset(
    client: &reqwest::Client,
    job_id: &str,
    index: usize,
    asset: &Asset,
    out_dir: &Path,
) -> anyhow::Result<PathBuf> {
    // 确保输出目录存在(三个分支都要落到 out_dir)
    tokio::fs::create_dir_all(out_dir).await?;

    // 穷尽 match 三种来源 + 空来源
    match asset.source() {
        AssetSource::Url(url) => {
            // 发起 GET, 一次性读入(MVP 足够; 后续可换 stream 边下边写省内存)
            let resp = client.get(url).send().await?;
            if !resp.status().is_success() {
                anyhow::bail!("下载失败 {}: HTTP {}", url, resp.status());
            }
            let bytes = resp.bytes().await?;
            let filename = build_filename(job_id, index, url, asset.kind);
            let dest = out_dir.join(filename);
            tokio::fs::write(&dest, &bytes).await?;
            Ok(dest)
        }
        AssetSource::Inline(inline) => {
            // 内联字节: ext 由 mime 推断, 直接写盘, 不打网络
            let ext = ext_from_mime(&inline.mime, asset.kind);
            let filename = build_filename_with_ext(job_id, index, &ext);
            let dest = out_dir.join(filename);
            tokio::fs::write(&dest, &inline.data).await?;
            Ok(dest)
        }
        AssetSource::LocalPath(src) => {
            // 已在本地: 复制进 out_dir。源恰好已落在 out_dir 内则免拷贝, 原样返回。
            let ext = ext_from_path(src, asset.kind);
            let filename = build_filename_with_ext(job_id, index, &ext);
            let dest = out_dir.join(&filename);
            // 同一路径(源已在目标处)无需复制, 避免自我覆盖
            if src == dest {
                return Ok(dest);
            }
            tokio::fs::copy(src, &dest).await.map_err(|e| {
                anyhow::anyhow!("复制本地素材失败 {} -> {}: {}", src.display(), dest.display(), e)
            })?;
            Ok(dest)
        }
        AssetSource::Empty => {
            anyhow::bail!("素材 #{} 无任何来源(url/inline/local_path 皆空), 无法落盘", index)
        }
    }
}

/// 下载一个 Job 的全部产物, 返回落盘路径列表。
pub async fn download_job_outputs(
    client: &reqwest::Client,
    job: &Job,
    out_dir: &Path,
) -> anyhow::Result<Vec<PathBuf>> {
    let mut saved = Vec::with_capacity(job.outputs.len());
    for (index, asset) in job.outputs.iter().enumerate() {
        let path = download_asset(client, &job.id, index, asset, out_dir).await?;
        saved.push(path);
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filename_uses_url_ext() {
        let name = build_filename("abc123", 0, "https://cdn.fal.ai/x/out.png", AssetKind::Image);
        assert_eq!(name, "abc123_0.png");
    }

    #[test]
    fn filename_strips_query() {
        let name = build_filename(
            "job",
            2,
            "https://cdn.fal.ai/a/b.jpeg?token=xyz&v=1",
            AssetKind::Image,
        );
        assert_eq!(name, "job_2.jpeg");
    }

    #[test]
    fn filename_falls_back_to_kind_default() {
        // URL 末段没有合法扩展名 -> 用类型默认
        let img = build_filename("j", 0, "https://x/result", AssetKind::Image);
        assert_eq!(img, "j_0.png");
        let vid = build_filename("j", 1, "https://x/render/final", AssetKind::Video);
        assert_eq!(vid, "j_1.mp4");
        let aud = build_filename("j", 0, "https://x/clip", AssetKind::Audio);
        assert_eq!(aud, "j_0.mp3");
    }

    #[test]
    fn filename_rejects_too_long_pseudo_ext() {
        // 末段含点但点后是长串(非扩展名) -> 回退默认
        let name = build_filename("j", 0, "https://x/v1.2.3.somelongtail", AssetKind::Image);
        assert_eq!(name, "j_0.png");
    }

    #[test]
    fn mime_maps_to_ext() {
        // 常见图片 mime -> 扩展名
        assert_eq!(ext_from_mime("image/png", AssetKind::Image), "png");
        assert_eq!(ext_from_mime("image/jpeg", AssetKind::Image), "jpg");
        assert_eq!(ext_from_mime("image/webp", AssetKind::Image), "webp");
        // 带参数也能解析
        assert_eq!(ext_from_mime("image/png; charset=binary", AssetKind::Image), "png");
        // 未知 mime 回退到素材类型默认
        assert_eq!(ext_from_mime("application/x-unknown", AssetKind::Image), "png");
        assert_eq!(ext_from_mime("application/x-unknown", AssetKind::Video), "mp4");
    }

    #[tokio::test]
    async fn inline_asset_writes_decoded_bytes_with_mime_ext() {
        use crate::core::provider::Asset;

        // fixture: 1x1 透明 png 的 base64, 解码成原始字节后构造 InlineBytes 素材
        let b64 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M8AAAMBAQAY3Y2wAAAAAElFTkSuQmCC";
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
            .expect("fixture base64 应可解码");

        // 唯一临时目录, 避免并发测试互踩
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let out_dir = std::env::temp_dir().join(format!("imagecli_dl_{}_{}", std::process::id(), nanos));

        let asset = Asset::from_inline_bytes(AssetKind::Image, "image/png", bytes.clone());
        let client = reqwest::Client::new();
        let dest = download_asset(&client, "jobX", 0, &asset, &out_dir)
            .await
            .expect("内联字节应能落盘");

        // 文件名扩展名由 mime 推断为 png
        assert_eq!(dest.file_name().unwrap().to_str().unwrap(), "jobX_0.png");
        // 落盘字节与解码字节一致
        let written = tokio::fs::read(&dest).await.expect("应能读回落盘文件");
        assert_eq!(written, bytes);

        // 清理
        let _ = tokio::fs::remove_dir_all(&out_dir).await;
    }
}
