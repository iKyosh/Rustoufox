//! Locale validation and the statistical choice of a locale for a territory
//! or a language, from CLDR's territory data (`territoryInfo.xml`).

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use rand::Rng;
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::paths::data;
use crate::warnings;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Locale {
    pub language: String,
    pub region: Option<String>,
    pub script: Option<String>,
}

impl Locale {
    pub fn as_string(&self) -> String {
        match &self.region {
            Some(r) => format!("{}-{r}", self.language),
            None => self.language.clone(),
        }
    }

    /// The `locale:*` config keys. Requires a region.
    pub fn as_config(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("locale:region".into(), self.region.clone().unwrap_or_default().into());
        m.insert("locale:language".into(), self.language.clone().into());
        if let Some(s) = &self.script {
            m.insert("locale:script".into(), s.clone().into());
        }
        m
    }
}

#[derive(Debug, Clone)]
pub struct Geolocation {
    pub locale: Locale,
    pub longitude: f64,
    pub latitude: f64,
    pub timezone: String,
    pub accuracy: Option<f64>,
}

impl Geolocation {
    pub fn as_config(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("geolocation:longitude".into(), Value::from(self.longitude));
        m.insert("geolocation:latitude".into(), Value::from(self.latitude));
        m.insert("timezone".into(), self.timezone.clone().into());
        m.extend(self.locale.as_config());
        if let Some(a) = self.accuracy.filter(|a| *a != 0.0) {
            m.insert("geolocation:accuracy".into(), Value::from(a));
        }
        m
    }
}

/// The IANA subtag registry, reduced to what locale validation needs.
#[derive(Debug, Deserialize)]
struct Registry {
    /// language subtag -> its Suppress-Script ("" when none)
    languages: HashMap<String, String>,
    regions: Vec<String>,
    scripts: Vec<String>,
}

struct RegistryIndex {
    languages: HashMap<String, (String, String)>,
    regions: HashMap<String, String>,
    scripts: HashMap<String, String>,
}

fn registry() -> &'static RegistryIndex {
    static R: OnceLock<RegistryIndex> = OnceLock::new();
    R.get_or_init(|| {
        let r: Registry = serde_json::from_str(data::LANGUAGE_REGISTRY).expect("language-registry.json");
        RegistryIndex {
            languages: r.languages.into_iter().map(|(k, v)| (k.to_lowercase(), (k, v))).collect(),
            regions: r.regions.into_iter().map(|k| (k.to_lowercase(), k)).collect(),
            scripts: r.scripts.into_iter().map(|k| (k.to_lowercase(), k)).collect(),
        }
    })
}

/// A parsed `language[-script][-region]` tag, in the registry's case.
struct Tag {
    language: String,
    suppress_script: Option<String>,
    region: Option<String>,
}

fn parse_tag(locale: &str) -> Option<Tag> {
    let reg = registry();
    let mut parts = locale.split(['-', '_']);
    let (language, suppress) = reg.languages.get(&parts.next()?.to_lowercase())?.clone();
    let mut region = None;
    let mut seen_script = false;
    for part in parts {
        let lower = part.to_lowercase();
        if region.is_none() && !seen_script && part.len() == 4 && reg.scripts.contains_key(&lower) {
            seen_script = true;
        } else if region.is_none() && reg.regions.contains_key(&lower) && (part.len() == 2 || part.len() == 3) {
            region = Some(reg.regions[&lower].clone());
        } else {
            return None;
        }
    }
    Some(Tag { language, suppress_script: (!suppress.is_empty()).then_some(suppress), region })
}

fn invalid(locale: &str) -> Error {
    Error::InvalidLocale(format!(
        "Invalid locale: '{locale}'. Must be either a region, language, language-region, or language-script-region."
    ))
}

pub fn verify_locale(locale: &str) -> Result<()> {
    parse_tag(locale).map(|_| ()).ok_or_else(|| invalid(locale))
}

/// Normalize and validate `language-region` or `language-script-region`.
pub fn normalize_locale(locale: &str) -> Result<Locale> {
    let tag = parse_tag(locale).ok_or_else(|| invalid(locale))?;
    let region = tag.region.ok_or_else(|| invalid(locale))?;
    Ok(Locale { language: tag.language, region: Some(region), script: tag.suppress_script })
}

/// A region, a language, or a full locale, as a Locale.
pub fn handle_locale(locale: &str, ignore_region: bool) -> Result<Locale> {
    if locale.len() > 3 {
        return normalize_locale(locale);
    }
    match selector().from_region(locale) {
        Ok(l) => return Ok(l),
        Err(Error::UnknownTerritory(_)) => {}
        Err(e) => return Err(e),
    }
    if ignore_region {
        verify_locale(locale)?;
        return Ok(Locale { language: locale.to_string(), region: None, script: None });
    }
    match selector().from_language(locale) {
        Ok(l) => {
            warnings::leak("no_region", None);
            Ok(l)
        }
        Err(Error::UnknownLanguage(_)) => Err(invalid(locale)),
        Err(e) => Err(e),
    }
}

/// Apply one or more locales (the first is used for Intl) to a config.
pub fn handle_locales(locales: &[String], config: &mut Map<String, Value>) -> Result<()> {
    let locales: Vec<String> = if locales.len() == 1 {
        locales[0].split(',').map(|l| l.trim().to_string()).collect()
    } else {
        locales.to_vec()
    };
    let Some(first) = locales.first() else { return Ok(()) };
    config.extend(handle_locale(first, false)?.as_config());
    if locales.len() < 2 {
        return Ok(());
    }
    let mut seen = HashSet::new();
    let mut all = Vec::new();
    for l in &locales {
        let s = handle_locale(l, true)?.as_string();
        if seen.insert(s.clone()) {
            all.push(s);
        }
    }
    config.insert("locale:all".into(), all.join(", ").into());
    Ok(())
}

struct Territory {
    population: f64,
    literacy: f64,
    languages: Vec<(String, f64)>,
}

/// Picks a locale for a territory or a language by population share.
pub struct LocaleSelector {
    territories: Vec<(String, Territory)>,
}

pub fn selector() -> &'static LocaleSelector {
    static S: OnceLock<LocaleSelector> = OnceLock::new();
    S.get_or_init(LocaleSelector::load)
}

fn weighted(items: &[(String, f64)]) -> Option<&str> {
    let total: f64 = items.iter().map(|i| i.1).sum();
    if total <= 0.0 {
        return None;
    }
    let mut r = rand::thread_rng().gen::<f64>() * total;
    for (v, w) in items {
        r -= w;
        if r < 0.0 {
            return Some(v);
        }
    }
    items.last().map(|i| i.0.as_str())
}

impl LocaleSelector {
    fn load() -> Self {
        let doc = roxmltree::Document::parse_with_options(
            data::TERRITORY_INFO,
            roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() },
        )
        .expect("territoryInfo.xml");
        let num = |n: &roxmltree::Node, a: &str| n.attribute(a).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let territories = doc
            .root_element()
            .children()
            .filter(|n| n.has_tag_name("territory"))
            .filter_map(|t| {
                let code = t.attribute("type")?.to_string();
                let languages = t
                    .children()
                    .filter(|n| n.has_tag_name("languagePopulation"))
                    .filter_map(|l| Some((l.attribute("type")?.to_string(), num(&l, "populationPercent"))))
                    .collect();
                Some((
                    code,
                    Territory { population: num(&t, "population"), literacy: num(&t, "literacyPercent"), languages },
                ))
            })
            .collect();
        LocaleSelector { territories }
    }

    /// A locale for a territory ISO code, its language drawn by speaker share.
    pub fn from_region(&self, region: &str) -> Result<Locale> {
        let territory = self
            .territories
            .iter()
            .find(|(c, _)| c == region)
            .map(|(_, t)| t)
            .ok_or_else(|| Error::UnknownTerritory(region.to_string()))?;
        if territory.languages.is_empty() {
            return Err(Error::value(format!("No language data found for region: {region}")));
        }
        let language = weighted(&territory.languages)
            .ok_or_else(|| Error::value(format!("No language data found for region: {region}")))?;
        normalize_locale(&format!("{}-{region}", language.replace('_', "-")))
    }

    /// A locale for a language, its region drawn by total speakers.
    pub fn from_language(&self, language: &str) -> Result<Locale> {
        let regions: Vec<(String, f64)> = self
            .territories
            .iter()
            .filter_map(|(code, t)| {
                let pct = t.languages.iter().find(|(l, _)| l == language)?.1;
                Some((code.clone(), pct * t.literacy / 10_000.0 * t.population))
            })
            .collect();
        if regions.is_empty() {
            return Err(Error::UnknownLanguage(language.to_string()));
        }
        let region = weighted(&regions)
            .ok_or_else(|| Error::value(format!("No valid region data found for language: {language}")))?;
        normalize_locale(&format!("{language}-{region}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes() {
        let l = normalize_locale("fr-fr").unwrap();
        assert_eq!((l.language.as_str(), l.region.as_deref(), l.script.as_deref()), ("fr", Some("FR"), Some("Latn")));
        assert!(normalize_locale("en").is_err());
        assert!(normalize_locale("zz-ZZ").is_err());
        assert_eq!(normalize_locale("zh-Hant-TW").unwrap().region.as_deref(), Some("TW"));
    }

    #[test]
    fn region_and_language() {
        let l = handle_locale("US", false).unwrap();
        assert_eq!(l.region.as_deref(), Some("US"));
        let l = selector().from_language("de").unwrap();
        assert_eq!(l.language, "de");
    }

    #[test]
    fn several_locales() {
        let mut c = Map::new();
        handle_locales(&["fr-FR, en-US, fr".to_string()], &mut c).unwrap();
        assert_eq!(c["locale:language"], "fr");
        assert_eq!(c["locale:all"], "fr-FR, en-US, fr");
    }
}
