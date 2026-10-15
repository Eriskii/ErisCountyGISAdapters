//! `spec`: validate adapter specs, look at the last one, save it, or record that a county has
//! no usable service. Statewide searches get the same command as `statewide_spec`, without
//! `unavailable`.

use crate::cli;
use crate::journal::Job;
use crate::registry::now;
use crate::spec::{Reason, SPEC_VERSION, Spec, Unavailable};
use crate::work::{Resolution, Validated, Work};
use clap::builder::PossibleValuesParser;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use erisharness::tools::{Tool, ToolContext, ToolOutput};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::sync::Arc;

pub const STATEWIDE_SPEC: &str = "statewide_spec";
const DAY_MS: u64 = 86_400_000;

const FORMAT: &str = r#"A spec is JSON:

{
  "id": "ga-fulton",                       lowercase letters, digits, dashes; unique
  "publisher": "Fulton County GIS",        who publishes the layer
  "source": {"kind": "arcgis-feature", "url": "https://…/arcgis/rest/services/…/MapServer/0",
             "page_size": 1000,            the layer's maxRecordCount, or less
             "geojson": true},             false when supportedQueryFormats lacks geoJSON
  "coverage": {"counties": ["13121"]},     or {"states": ["WI"]} for a statewide layer
  "fields": {"apn": "PARCELID", "owner": "OWNERNME1", "address": "SITEADRESS",
             "acres": "GISACRES", "land_use": "PROPCLASS", "assessed_value": "CNTASSDVALUE"},
  "filter": "COUNTY_NAME = 'FULTON'",      optional ArcGIS where clause
  "notes": "CC-BY 4.0; updated weekly"     license, terms, quirks
}

Other sources:
  {"kind": "wfs", "url": "https://…/geoserver/wfs", "type_name": "ns:parcels", "version": "2.0.0",
   "page_size": 1000, "lat_lon": true}    lat_lon: the server wants EPSG:4326 latitude first
  {"kind": "ogc-features", "url": "https://…/collections/parcels", "page_size": 1000}
  {"kind": "bulk-download", "url": "https://…/parcels.zip", "format": "shapefile-zip", "epsg": 4326}
                                           format geojson or shapefile-zip; epsg 4326, 4269 or 3857

Map apn, address or owner at least, and every field you can, with attribute names exactly as the
source spells them. The runtime asks for longitude/latitude and pages by itself.

Give the JSON as one argument in single quotes. Inside them, write a single quote as '\'' :
  spec validate '{"id": "x", …, "filter": "NAME = '\''FULTON'\''"}'"#;

#[derive(Parser)]
#[command(
    name = "spec",
    disable_version_flag = true,
    about = "Validate adapter specs and save the one that passes.",
    after_long_help = FORMAT
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a spec for real against your county (in a statewide search, against three counties
    /// spread across the state; two must pass) and print the report: each check, the parcels
    /// found, field fill rates, and the source's attribute names and sample attributes. Each
    /// failing validation uses one of your attempts.
    Validate {
        /// The spec as JSON, in single quotes.
        json: String,
    },
    /// Show the spec validated last and its report.
    Show,
    /// Save the spec validated last to the registry, if it passed. This ends your job.
    Submit,
    /// Record that your county publishes no usable parcel service, with links that show it.
    /// This ends your job; use it only after a real search.
    #[command(
        after_help = "Example: spec unavailable paywalled --evidence https://county.example/gis-data-sales --note \"Parcel data is sold per county\""
    )]
    Unavailable {
        #[arg(value_parser = PossibleValuesParser::new(Reason::ALL.map(Reason::name)))]
        reason: String,
        /// Links that show it: the viewer, the price list, the terms, the login page.
        #[arg(long, required = true, num_args = 1..)]
        evidence: Vec<String>,
        /// What you found, in a sentence or two.
        #[arg(long)]
        note: String,
    },
}

pub struct SpecTool {
    pub work: Arc<Work>,
    /// The statewide search's variant, which cannot mark counties unavailable.
    pub statewide: bool,
}

impl SpecTool {
    fn program(&self) -> &'static str {
        if self.statewide { STATEWIDE_SPEC } else { "spec" }
    }

    fn command(&self) -> clap::Command {
        let command = Cli::command().name(self.program());
        if self.statewide { command.mut_subcommand("unavailable", |c| c.hide(true)) } else { command }
    }

    fn parse(&self, line: &str) -> Result<Command, ToolOutput> {
        let program = self.program();
        let mut argv = cli::words(line).map_err(|e| ToolOutput::error(format!("{program}: {e}")))?;
        if argv.first().is_some_and(|w| w == program || w == "spec") {
            argv.remove(0);
        }
        argv.insert(0, program.into());
        let parsed = self.command().try_get_matches_from(argv).and_then(|m| Cli::from_arg_matches(&m));
        match parsed {
            Ok(cli) => Ok(cli.command),
            Err(error) => {
                let text = error.render().to_string();
                Err(if error.use_stderr() { ToolOutput::error(text) } else { ToolOutput::text(text) })
            }
        }
    }

    pub async fn run(&self, agent: &str, line: &str) -> ToolOutput {
        super::activity(&self.work, agent, self.program(), line);
        let command = match self.parse(line) {
            Ok(command) => command,
            Err(output) => return output,
        };
        let Some(job) = self.work.with_agent(agent, |a| a.job.clone()) else {
            return ToolOutput::error("this agent has no job");
        };
        match command {
            Command::Validate { json } => self.validate(agent, &job, &json).await,
            Command::Show => match self.work.with_agent(agent, |a| a.last.clone()).flatten() {
                Some(last) => ToolOutput::text(format!(
                    "{}\n\n{}",
                    serde_json::to_string_pretty(&last.spec).unwrap_or_default(),
                    last.report
                )),
                None => ToolOutput::error("no spec has been validated yet; run spec validate '<json>'"),
            },
            Command::Submit => self.submit(agent, &job),
            Command::Unavailable { reason, evidence, note } => {
                let Job::County(fips) = job else {
                    return ToolOutput::error("a statewide search cannot mark counties unavailable");
                };
                self.unavailable(agent, &fips, Reason::parse(&reason).expect("a listed reason"), evidence, note)
            }
        }
    }

    async fn validate(&self, agent: &str, job: &Job, text: &str) -> ToolOutput {
        let mut value: Value = match serde_json::from_str(text) {
            Ok(value) => value,
            Err(error) => {
                return ToolOutput::error(format!(
                    "the spec is not JSON ({error}). Give it as one argument in single quotes: spec validate '{{\"id\": …}}'"
                ));
            }
        };
        if value.is_object() && value.get("version").is_none() {
            value["version"] = json!(SPEC_VERSION);
        }
        let spec: Spec = match serde_json::from_value(value) {
            Ok(spec) => spec,
            Err(error) => return ToolOutput::error(format!("the spec does not read: {error}")),
        };
        let (passed, reports) = self.work.validate_for(job, &spec).await;
        let report = serde_json::to_string_pretty(&json!({"passed": passed, "reports": reports})).unwrap_or_default();
        let budget = self.work.validate_budget;
        let (failures, remaining) = self
            .work
            .with_agent(agent, |a| {
                for r in &reports {
                    a.note(r.summary());
                }
                if !passed {
                    a.failures += 1;
                }
                a.last = Some(Validated { spec: spec.clone(), passed, report: report.clone() });
                (a.failures, budget.saturating_sub(a.failures))
            })
            .unwrap_or((0, budget));
        let next = if passed {
            "The spec passes. Run `spec submit` to save it.".to_owned()
        } else if remaining == 0 {
            format!(
                "That was your last of {budget} failing validations. Stop now: reply with a short summary of \
                 what you tried (URLs, layers, what failed) so the next attempt can start from it."
            )
        } else {
            format!("{failures} failing validations so far; {remaining} left.")
        };
        ToolOutput::text(format!("{report}\n\n{next}"))
    }

    fn submit(&self, agent: &str, job: &Job) -> ToolOutput {
        let Some((last, model, rung)) = self.work.with_agent(agent, |a| (a.last.clone(), a.model.clone(), a.rung))
        else {
            return ToolOutput::error("this agent has no job");
        };
        let Some(last) = last else {
            return ToolOutput::error("no spec has been validated yet; run spec validate '<json>' first");
        };
        if !last.passed {
            return ToolOutput::error("the spec validated last did not pass; fix it and validate again");
        }
        let passing: Vec<String> = serde_json::from_str::<Value>(&last.report)
            .ok()
            .and_then(|r| {
                r["reports"].as_array().map(|reports| {
                    reports
                        .iter()
                        .filter(|r| r["passed"] == true)
                        .filter_map(|r| r["fips"].as_str().map(str::to_owned))
                        .collect()
                })
            })
            .unwrap_or_default();
        let at = now();
        let mut spec = last.spec;
        spec.verification.model = Some(model);
        spec.verification.rung = Some(rung);
        spec.verification.verified_at = at;
        spec.verification.counties = passing.clone();
        let saved = {
            let mut registry = self.work.registry.lock().unwrap();
            match registry.save_adapter(spec) {
                Ok(saved) => {
                    for fips in &passing {
                        if let Err(error) = registry.verified(&saved.id, fips, at) {
                            return ToolOutput::error(format!("saving failed: {error:#}"));
                        }
                    }
                    saved
                }
                Err(error) => return ToolOutput::error(format!("{error:#}")),
            }
        };
        self.work.with_agent(agent, |a| a.resolution = Some(Resolution::Covered(saved.id.clone())));
        self.work.log(format!("{}: saved {} (verified for {})", job.label(), saved.id, passing.join(", ")));
        ToolOutput::text(format!("Saved {}. Your job is done: reply with one line saying what you saved.", saved.id))
    }

    fn unavailable(&self, agent: &str, fips: &str, reason: Reason, evidence: Vec<String>, note: String) -> ToolOutput {
        let wait = match reason {
            Reason::Paywalled | Reason::TosForbidsAutomation => 365,
            _ => 180,
        };
        let at = now();
        let record = Unavailable {
            version: 1,
            fips: fips.to_owned(),
            reason,
            evidence,
            note,
            recorded_at: at,
            recheck_after: at + wait * DAY_MS,
            model: self.work.with_agent(agent, |a| a.model.clone()),
        };
        if let Err(error) = self.work.registry.lock().unwrap().mark_unavailable(record) {
            return ToolOutput::error(format!("{error:#}"));
        }
        self.work.with_agent(agent, |a| a.resolution = Some(Resolution::Unavailable(reason)));
        self.work.log(format!("county {fips}: unavailable ({})", reason.name()));
        ToolOutput::text("Recorded. Your job is done: reply with one line.")
    }
}

impl Tool for SpecTool {
    fn name(&self) -> &str {
        self.program()
    }
    fn description(&self) -> String {
        format!(
            "Runs a `{}` command, given as a command line.\n\n{}",
            self.program(),
            self.command().render_long_help()
        )
    }
    fn parameters(&self) -> Value {
        cli::parameters("validate '{\"id\": \"ga-fulton\", …}'")
    }
    fn snippet(&self) -> &str {
        if self.statewide {
            "statewide_spec: validate a statewide spec, and save it once it passes"
        } else {
            "spec: validate a spec, save it once it passes, or record that the county has no service"
        }
    }
    fn call<'a>(&'a self, context: &'a ToolContext, args: Value) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            match cli::line(&args, self.program(), "show") {
                Ok(line) => self.run(&context.agent, &line).await,
                Err(output) => output,
            }
        })
    }
}
