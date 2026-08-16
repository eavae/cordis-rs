//! Story card F4: artifact hash naming and the build manifest.
//!
//! The full `cordis build`/`cordis dev` CLI lands in the H-series; this
//! module provides the cache-busting core: content-hash artifact names
//! (`name@hash.so`) and a manifest mapping artifacts to their declared
//! dependencies (consumed by the F2 dependency graph).

use std::collections::BTreeMap;
use std::path::Path;

/// A plugin source entry to build.
#[derive(Clone, Debug)]
pub struct BuildTarget {
    /// Plugin name (registry key).
    pub name: String,
    /// Declared dependencies (host crates/services).
    pub deps: Vec<String>,
    /// The built artifact's file name (e.g. `libfoo.dylib`).
    pub artifact: String,
}

/// A manifest entry: artifact path plus its dependency declarations.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ManifestEntry {
    /// Plugin name.
    pub name: String,
    /// Content hash (short form) used in the artifact file name.
    pub hash: String,
    /// Artifact file name (`name@hash.so` / `name@hash.dylib`).
    pub artifact: String,
    /// Declared dependencies.
    #[serde(default)]
    pub deps: Vec<String>,
}

/// Computes a short content hash (SHA-256, 12 hex chars).
pub fn content_hash(content: &[u8]) -> String {
    let digest = sha256(content);
    digest[..12].to_string()
}

/// Builds the manifest for a set of targets whose artifacts exist in
/// `artifact_dir`. Returns the manifest sorted by plugin name.
pub fn build_manifest(
    targets: &[BuildTarget],
    artifact_dir: &Path,
) -> Result<Vec<ManifestEntry>, String> {
    let mut entries = BTreeMap::new();
    for target in targets {
        let path = artifact_dir.join(&target.artifact);
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("cannot read artifact {}: {error}", path.display()))?;
        let hash = content_hash(&bytes);
        let extension = Path::new(&target.artifact)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("so");
        entries.insert(
            target.name.clone(),
            ManifestEntry {
                name: target.name.clone(),
                hash: hash.clone(),
                artifact: format!("{}@{hash}.{extension}", target.name),
                deps: target.deps.clone(),
            },
        );
    }
    Ok(entries.into_values().collect())
}

/// The planned artifact name for a target (`name@hash.ext`).
pub fn planned_artifact(target: &BuildTarget, artifact_dir: &Path) -> Result<String, String> {
    let bytes = std::fs::read(artifact_dir.join(&target.artifact))
        .map_err(|error| format!("cannot read artifact: {error}"))?;
    let hash = content_hash(&bytes);
    let extension = Path::new(&target.artifact)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("so");
    Ok(format!("{}@{hash}.{extension}", target.name))
}

/// A tiny SHA-256 implementation (FIPS 180-4) so the build module has no
/// external hashing dependency.
fn sha256(input: &[u8]) -> String {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut message = input.to_vec();
    let bit_len = (message.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in message.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            w[index] = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }
        let mut a = state[0];
        let mut b = state[1];
        let mut c = state[2];
        let mut d = state[3];
        let mut e = state[4];
        let mut f = state[5];
        let mut g = state[6];
        let mut h = state[7];
        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }

    state
        .iter()
        .map(|word| format!("{word:08x}"))
        .collect::<Vec<_>>()
        .join("")
}

/// Result type alias for the build module.
pub type BuildResult<T> = Result<T, String>;

#[allow(dead_code)]
fn _path_check(path: &Path) -> bool {
    path.is_absolute()
}
