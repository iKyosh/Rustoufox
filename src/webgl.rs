//! WebGL identities, drawn from the Firefox devices fpgen has recorded.
//!
//! Everything a page can read from WebGL comes from one recorded device:
//! fpgen's `webgl` node is conditioned on the GPU, and `webgl2` on the GPU and
//! the `webgl` chosen for it, so a WebGL2 limit never contradicts its WebGL1
//! counterpart. Every draw takes one generator in a fixed order (GPU, webgl,
//! webgl2), so a seeded identity always presents the same device.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::coherence::{gpu_fits_machine, gpu_fits_os};
use crate::error::{Error, Result};
use crate::fingerprints::{fpgen_os, gpu_screen_is_plausible, is_software_renderer};
use crate::fpgen::{self, Condition, Evidence, TraceResult};
use crate::pyrandom::PyRandom;

/// Extensions a release Firefox never exposes.
const NEVER_EXPOSED_EXTENSIONS: [&str; 4] =
    ["WEBGL_multi_draw", "WEBGL_clip_cull_distance", "EXT_texture_norm16", "WEBGL_compressed_texture_etc1"];
/// Exposed only where the host's GL driver backs it; kept on Windows (ANGLE D3D11 always does).
const HOST_DEPENDENT_EXTENSIONS: [&str; 1] = ["OVR_multiview2"];
const ALIASED_LINE_WIDTH_RANGE: &str = "33902";
/// What Firefox reports under privacy.resistFingerprinting.
const RFP_RENDERER: &str = "Mozilla";

fn filtered_extensions(os: &str) -> HashSet<&'static str> {
    let mut s: HashSet<&str> = NEVER_EXPOSED_EXTENSIONS.into_iter().collect();
    if os != "win" {
        s.extend(HOST_DEPENDENT_EXTENSIONS);
    }
    s
}

/// Extensions the claimed OS's backend exposes on every GPU in Firefox 156.
fn backend_extensions(os: &str) -> &'static [&'static str] {
    if os == "win" {
        &["EXT_depth_clamp"]
    } else {
        &[]
    }
}

type TraceKey = (String, String, Vec<(String, u32)>);

fn trace(target: &str, os: &str, pinned: &[(&str, u32)]) -> Result<Arc<Vec<TraceResult>>> {
    static CACHE: Mutex<Option<HashMap<TraceKey, Arc<Vec<TraceResult>>>>> = Mutex::new(None);
    let key: TraceKey = (target.into(), os.into(), pinned.iter().map(|(n, i)| (n.to_string(), *i)).collect());
    if let Some(hit) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return Ok(hit.clone());
    }
    let network = fpgen::network()?;
    let os_name = fpgen_os(os).ok_or_else(|| Error::InvalidOs(format!("Unknown OS {os:?}")))?;
    let mut evidence = Evidence::new();
    for (node, id) in pinned {
        let index = network.node_index(node).ok_or_else(|| Error::InvalidNode(format!("Unknown node {node}")))?;
        evidence.insert(index, HashSet::from([*id]));
    }
    let conditions = [("browser", Condition::Equals("Firefox".into())), ("os", Condition::Equals(os_name.into()))];
    let result = Arc::new(network.trace(target, &conditions, &evidence)?);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, result.clone());
    Ok(result)
}

fn choose<'a>(rng: &mut PyRandom, results: &'a [&'a TraceResult]) -> &'a TraceResult {
    let weights: Vec<f64> = results.iter().map(|r| r.probability).collect();
    results[rng.choices_index(&weights)]
}

/// Every (vendor, renderer) fpgen has seen Firefox report on this OS.
pub fn firefox_gpus(os: &str) -> Result<BTreeSet<(String, String)>> {
    Ok(trace("gpu", os, &[])?
        .iter()
        .filter_map(|r| Some((r.value["vendor"].as_str()?.to_string(), r.value["renderer"].as_str()?.to_string())))
        .collect())
}

fn context_config(prefix: &str, webgl: &Value, os: &str, out: &mut Map<String, Value>) {
    let blocked = filtered_extensions(os);
    let mut extensions: Vec<String> = webgl["supportedExtensions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|e| !blocked.contains(e))
        .map(str::to_string)
        .collect();
    for ext in backend_extensions(os) {
        if !extensions.iter().any(|e| e == ext) {
            // Firefox lists them in WebGLExtensionID order: alphabetical, ignoring case.
            let at = extensions.iter().position(|e| e.to_lowercase() > ext.to_lowercase()).unwrap_or(extensions.len());
            extensions.insert(at, ext.to_string());
        }
    }
    let params: Map<String, Value> = webgl["params"]
        .as_object()
        .map(|p| p.iter().map(|(k, v)| (k.clone(), v["value"].clone())).collect())
        .unwrap_or_default();
    let precisions: Map<String, Value> = webgl["shaderPrecisionFormats"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| {
            (
                format!(
                    "{},{}",
                    crate::pycompat::py_str(&e["shaderType"]),
                    crate::pycompat::py_str(&e["precisionType"])
                ),
                e["shaderPrecisionFormat"].clone(),
            )
        })
        .collect();
    out.insert(format!("{prefix}:supportedExtensions"), extensions.into());
    out.insert(format!("{prefix}:parameters"), params.into());
    out.insert(format!("{prefix}:shaderPrecisionFormats"), precisions.into());
}

fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// fpgen's `webgl` and `webgl2` values as Camoufox config keys, plus
/// `webGl2Enabled`. `webgl2` is empty for a device without WebGL2.
pub fn to_config(webgl: &Value, webgl2: &Value, os: &str) -> Map<String, Value> {
    let mut config = Map::new();
    config.insert("webGl:vendor".into(), webgl["vendor"].clone());
    config.insert("webGl:renderer".into(), webgl["renderer"].clone());
    context_config("webGl", webgl, os, &mut config);
    config.insert("webGl2Enabled".into(), (!is_empty_value(webgl2)).into());
    if !is_empty_value(webgl2) {
        context_config("webGl2", webgl2, os, &mut config);
        // Firefox 156 creates WebGL1 on the same GL profile as WebGL2, so both report one line width range.
        if let Some(range) = config["webGl2:parameters"].get(ALIASED_LINE_WIDTH_RANGE).cloned() {
            if let Some(p) = config.get_mut("webGl:parameters").and_then(Value::as_object_mut) {
                p.insert(ALIASED_LINE_WIDTH_RANGE.into(), range);
            }
        }
    }
    config
}

fn gpu_result(os: &str, vendor: &str, renderer: &str) -> Result<TraceResult> {
    trace("gpu", os, &[])?
        .iter()
        .find(|r| r.value["vendor"].as_str() == Some(vendor) && r.value["renderer"].as_str() == Some(renderer))
        .cloned()
        .ok_or_else(|| {
            let pairs = firefox_gpus(os).map(|g| format!("{g:?}")).unwrap_or_default();
            Error::value(format!(
                "No recorded WebGL data for vendor {vendor:?} and renderer {renderer:?} from Firefox on {}. Possible pairs: {pairs}",
                fpgen_os(os).unwrap_or(os)
            ))
        })
}

/// Every device fpgen recorded behind this GPU from Firefox on `os`.
pub fn recorded_devices(os: &str, vendor: &str, renderer: &str) -> Result<Vec<Map<String, Value>>> {
    let gpu = gpu_result(os, vendor, renderer)?;
    let mut out = Vec::new();
    for webgl in trace("webgl", os, &[("gpu", gpu.id)])?.iter() {
        for webgl2 in trace("webgl2", os, &[("gpu", gpu.id), ("webgl", webgl.id)])?.iter() {
            out.push(to_config(&webgl.value, &webgl2.value, os));
        }
    }
    Ok(out)
}

fn webgl_config(os: &str, gpu: &TraceResult, rng: &mut PyRandom) -> Result<Map<String, Value>> {
    let webgls = trace("webgl", os, &[("gpu", gpu.id)])?;
    let refs: Vec<&TraceResult> = webgls.iter().collect();
    let webgl = choose(rng, &refs).clone();
    let webgl2s = trace("webgl2", os, &[("gpu", gpu.id), ("webgl", webgl.id)])?;
    let refs2: Vec<&TraceResult> = webgl2s.iter().collect();
    let webgl2 = choose(rng, &refs2);
    Ok(to_config(&webgl.value, &webgl2.value, os))
}

/// The WebGL config of a device with this GPU, as Firefox on `os` reports it.
/// Fails for a GPU fpgen never saw Firefox report there.
pub fn webgl_for_gpu(os: &str, vendor: &str, renderer: &str, seed: Option<u32>) -> Result<Map<String, Value>> {
    let gpu = gpu_result(os, vendor, renderer)?;
    webgl_config(os, &gpu, &mut PyRandom::new(seed.map(u128::from)))
}

/// Draw a GPU a synthetic identity can stand beside, weighted as fpgen
/// records Firefox on `os`, and its WebGL config.
pub fn sample_webgl_for_screen(
    os: &str,
    width: Option<i64>,
    height: Option<i64>,
    seed: Option<u32>,
    cores: Option<i64>,
) -> Result<Map<String, Value>> {
    let gpus = trace("gpu", os, &[])?;
    let candidates: Vec<&TraceResult> = gpus
        .iter()
        .filter(|r| {
            let renderer = r.value["renderer"].as_str().unwrap_or("");
            !is_software_renderer(renderer)
                && renderer != RFP_RENDERER
                && gpu_fits_os(renderer, os)
                && gpu_fits_machine(renderer, os, cores, width, height)
                && gpu_screen_is_plausible(renderer, width, height)
        })
        .collect();
    if candidates.is_empty() {
        return Err(Error::value(format!(
            "No recorded {os} GPU fits a {}x{} screen",
            width.map(|w| w.to_string()).unwrap_or("None".into()),
            height.map(|h| h.to_string()).unwrap_or("None".into())
        )));
    }
    let mut rng = PyRandom::new(seed.map(u128::from));
    let gpu = choose(&mut rng, &candidates).clone();
    webgl_config(os, &gpu, &mut rng)
}
