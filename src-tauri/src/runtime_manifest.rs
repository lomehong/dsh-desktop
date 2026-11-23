//! 运行时完整性清单：给「应用自带运行时」的关键文件记一份 {path, size, sha256}
//! 清单，安装完成后写入、启动时校验。
//!
//! 对照官方 desktop 的 `desktop-runtime.json`（他们逐文件哈希，并把核心包钉成本地
//! tarball，从结构上不存在「npm 发布窗口期」问题）。我们做不到钉本地 tarball
//! （运行时来自 npm），但可以拿到同样的**可验证性**：
//!
//! - 装完即盘（`<root>/runtime-manifest.json`）；
//! - 启动时抽样校验关键文件（node.exe / npm-cli.js / dsh bin.js / 关键子包入口），
//!   大小 + sha256 对不上 = 安装被截断或文件被替换/损坏（真实故障类：镜像 tarball
//!   缺文件、下载中断留下半个包、杀毒软件隔离了某个 .js）。
//!
//! 为什么**不阻断启动**：清单只覆盖关键文件，全量哈希会让每次启动多花几百毫秒到
//! 数秒；而真正的故障（文件缺失）本来就会被 `ready_root` 三文件检查拦住。这里
//! 的角色是「给出可读证据 + 触发一次自愈」，而不是当启动门禁。
//!
//! 为什么不校验插件（profile 内第三方包）：它们由 pnpm 管理、每次装插件都会变，
//! 不该进这份清单；官方也是只覆盖随包分发的核心依赖树。
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 清单文件名（放在运行时根）。
pub const MANIFEST_FILE: &str = "runtime-manifest.json";

/// 清单 schema 版本：结构变更时递增，旧清单按「不存在」处理（不阻断）。
const SCHEMA_VERSION: u32 = 1;

/// 清单覆盖的关键文件（相对运行时根，正斜杠分隔）。
/// 只放「缺了/坏了就一定起不来」的入口，不放 node_modules 全量。
fn key_files(root: &Path) -> Vec<PathBuf> {
    let node_modules = root
        .join("node")
        .join(if cfg!(windows) { "node_modules" } else { "lib/node_modules" });
    let mut files = vec![
        root.join("node").join(if cfg!(windows) { "node.exe" } else { "bin/node" }),
        node_modules.join("npm").join("bin").join("npm-cli.js"),
        node_modules
            .join("@deepseek-ai")
            .join("dsh")
            .join("lib")
            .join("bin.js"),
    ];
    // dsh 主包的几个关键子包入口：主/子版本混装或子包缺失会表现为「启动即崩」
    for pkg in [
        "dsh-app-boot",
        "dsh-host-webserver",
        "dsh-web-app",
        "dsh-api-gateway",
    ] {
        files.push(
            node_modules
                .join("@deepseek-ai")
                .join("dsh")
                .join("node_modules")
                .join("@deepseek-ai")
                .join(pkg)
                .join("lib")
                .join("index.js"),
        );
    }
    files
}

/// 相对运行时根的路径表示（统一正斜杠，拒绝绝对路径/上跳）。
fn relative(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let text = rel.components()
        .map(|c| c.as_os_str().to_string_lossy().to_string())
        .collect::<Vec<_>>()
        .join("/");
    if text.is_empty() || text.contains("..") { None } else { Some(text) }
}

/// 解析相对路径（拒绝绝对路径与上跳，防清单被篡改成读任意文件）。
pub fn resolve(root: &Path, rel: &str) -> Option<PathBuf> {
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') || rel.contains(':') {
        return None;
    }
    if rel.split('/').any(|p| p.is_empty() || p == "." || p == "..") {
        return None;
    }
    Some(root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema: u32,
    /// 写入时刻（unix 秒）+ 当时的 dsh 版本，便于人读与排障
    pub written_at: u64,
    pub dsh_version: String,
    pub entries: Vec<Entry>,
}

/// 算一个文件的 {bytes, sha256}。读不到返回 None（不阻断写清单）。
fn fingerprint(path: &Path) -> Option<(u64, String)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Some((total, hasher.finish_hex()))
}

/// 生成并把清单写入运行时根。返回写入了多少条目（缺文件的不入清单）。
pub fn write(root: &Path, dsh_version: &str) -> Result<usize, String> {
    let mut entries = Vec::new();
    for path in key_files(root) {
        let Some(rel) = relative(root, &path) else { continue };
        let Some((bytes, sha256)) = fingerprint(&path) else { continue };
        entries.push(Entry { path: rel, bytes, sha256 });
    }
    let manifest = Manifest {
        schema: SCHEMA_VERSION,
        written_at: crate::runtime::unix_now(),
        dsh_version: dsh_version.to_string(),
        entries,
    };
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    crate::runtime::write_atomic(&root.join(MANIFEST_FILE), text.as_bytes())
        .map_err(|e| format!("写运行时清单失败: {e}"))?;
    Ok(manifest.entries.len())
}

/// 校验结果：缺失的清单不校验（返回 Ok(None)）；有清单则逐条比对。
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 没有清单（旧安装/首次装）——不视为故障
    NoManifest,
    /// 清单与磁盘一致
    Ok(usize),
    /// 有文件与清单不符：返回人读描述
    Mismatch(String),
}

/// 校验（只读）。容忍清单损坏（按 NoManifest 处理，不阻断启动）。
pub fn verify(root: &Path) -> Verdict {
    let path = root.join(MANIFEST_FILE);
    let Ok(text) = std::fs::read_to_string(&path) else { return Verdict::NoManifest };
    let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else { return Verdict::NoManifest };
    if manifest.schema != SCHEMA_VERSION {
        return Verdict::NoManifest; // 版本不同：旧清单，重装后自会更新
    }
    let mut checked = 0usize;
    for entry in &manifest.entries {
        let Some(abs) = resolve(root, &entry.path) else {
            return Verdict::Mismatch(format!("清单含非法路径：{}", entry.path));
        };
        match fingerprint(&abs) {
            Some((bytes, sha)) => {
                if bytes != entry.bytes || sha != entry.sha256 {
                    return Verdict::Mismatch(format!(
                        "{} 与清单不符（size {}→{bytes}，sha256 {}{}…）",
                        entry.path,
                        entry.bytes,
                        &entry.sha256[..8.min(entry.sha256.len())],
                        &sha[..8.min(sha.len())]
                    ));
                }
                checked += 1;
            }
            None => {
                return Verdict::Mismatch(format!("{} 缺失（清单记录 {} 字节）", entry.path, entry.bytes));
            }
        }
    }
    Verdict::Ok(checked)
}

/* ── 极简 SHA-256（零新增依赖；运行时清单只需要 sha256，别为它引入 crate） ── */

/// 最小 SHA-256 实现（FIPS 180-4）。输入是文件字节流，输出小写十六进制。
struct Sha256 {
    state: [u32; 8],
    buf: [u8; 64],
    buf_len: usize,
    total_len: u64,
}

impl Sha256 {
    fn new() -> Self {
        Self {
            state: [
                0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
            ],
            buf: [0; 64],
            buf_len: 0,
            total_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.total_len = self.total_len.wrapping_add(data.len() as u64);
        if self.buf_len > 0 {
            let need = 64 - self.buf_len;
            let take = need.min(data.len());
            self.buf[self.buf_len..self.buf_len + take].copy_from_slice(&data[..take]);
            self.buf_len += take;
            data = &data[take..];
            if self.buf_len == 64 {
                let block = self.buf;
                self.compress(&block);
                self.buf_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buf[..data.len()].copy_from_slice(data);
            self.buf_len = data.len();
        }
    }

    fn finish_hex(mut self) -> String {
        let bit_len = self.total_len.wrapping_mul(8);
        // padding：0x80 + 若干 0 + 8 字节长度
        let mut pad = vec![0x80u8];
        let rem = (self.buf_len + 1) % 64;
        let zeros = if rem <= 56 { 56 - rem } else { 120 - rem };
        pad.extend(std::iter::repeat(0).take(zeros));
        pad.extend_from_slice(&bit_len.to_be_bytes());
        // update 会追加 total_len，这里要避免影响最终长度字段——先把总长存回
        let saved = self.total_len;
        self.update(&pad);
        self.total_len = saved;
        let mut out = String::with_capacity(64);
        for word in self.state {
            out.push_str(&format!("{word:08x}"));
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        const K: [u32; 64] = [
            0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
            0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
            0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
            0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
            0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
            0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
            0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
            0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
        ];
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([block[4 * i], block[4 * i + 1], block[4 * i + 2], block[4 * i + 3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g; g = f; f = e; e = d.wrapping_add(t1);
            d = c; c = b; b = a; a = t1.wrapping_add(t2);
        }
        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SHA-256 必须与标准测试向量一致——清单的价值全在哈希正确性上。
    /// 向量由 node:crypto 独立算出（不是抄文档，避免抄错）。
    #[test]
    fn sha256_matches_known_vectors() {
        let hex = |bytes: &[u8]| {
            let mut h = Sha256::new();
            h.update(bytes);
            h.finish_hex()
        };
        assert_eq!(hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        // 恰好 64 字节（整块compress 分支）
        let a64 = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(hex(a64), "a8ae6e6ee929abea3afcfc5258c8ccd6f85273e0d4626d26c7279f3250f77c8e");
        // 55/56 字节：padding 的临界（56 会让长度字段跨块）
        assert_eq!(hex(&[0u8; 55]), "02779466cdec163811d078815c633f21901413081449002f24aa3e80f0b88ef7");
        assert_eq!(hex(&[0u8; 56]), "d4817aa5497628e7c77e6b606107042bbba3130888c5f47a375e6179be789fbb");
        // 多块输入
        assert_eq!(hex(&[0u8; 1000]), "541b3e9daa09b20bf85fa273e5cbd3e80185aa4ec298e765db87742b70138a53");
    }

    /// 清单写入 → 校验应一致；改动任一文件后应报不符；缺失文件应报缺失。
    #[test]
    fn manifest_roundtrip_detects_tampering() {
        let root = std::env::temp_dir().join(format!("dsh-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // 造出清单覆盖的关键文件形状（内容随意，只要可读）
        let files = key_files(&root);
        for (i, f) in files.iter().enumerate() {
            if let Some(dir) = f.parent() {
                std::fs::create_dir_all(dir).unwrap();
            }
            std::fs::write(f, format!("content-{i}")).unwrap();
        }
        assert!(matches!(verify(&root), Verdict::NoManifest));
        let n = write(&root, "0.2.3-alpha.2").unwrap();
        assert_eq!(n, files.len(), "所有关键文件都应入清单");
        assert_eq!(verify(&root), Verdict::Ok(n));
        // 改一个字节 → 不符
        let victim = &files[2];
        std::fs::write(victim, "tampered").unwrap();
        match verify(&root) {
            Verdict::Mismatch(msg) => assert!(msg.contains("不符"), "描述应说明不符: {msg}"),
            other => panic!("应报不符，实际 {other:?}"),
        }
        // 删掉一个 → 缺失
        let _ = std::fs::remove_file(victim);
        match verify(&root) {
            Verdict::Mismatch(msg) => assert!(msg.contains("缺失"), "描述应说明缺失: {msg}"),
            other => panic!("应报缺失，实际 {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 清单里的非法路径（绝对路径/上跳）必须被拒绝，不能变成任意文件读取。
    #[test]
    fn resolve_rejects_path_traversal() {
        let root = Path::new("/safe/root");
        assert!(resolve(root, "../etc/passwd").is_none());
        assert!(resolve(root, "a/../../b").is_none());
        assert!(resolve(root, "/abs/path").is_none());
        assert!(resolve(root, "C:/windows").is_none());
        assert!(resolve(root, "").is_none());
        assert!(resolve(root, "node/node.exe").is_some());
    }
}