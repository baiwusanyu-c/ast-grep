use ast_grep_config::{RuleConfig, Severity};
use ast_grep_core::{MetaVariable, Node, NodeMatch};
use ast_grep_language::SupportLang;
use std::collections::HashMap;

use super::{Diff, Printer};
use anyhow::Result;
pub use codespan_reporting::{files::SimpleFile, term::ColorArg};
use serde::{Deserialize, Serialize};

use std::borrow::Cow;
use std::io::{Stdout, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

// add this macro because neither trait_alias nor type_alias_impl is supported.
macro_rules! Matches {
  ($lt: lifetime) => { impl Iterator<Item = NodeMatch<$lt, SupportLang>> };
}
macro_rules! Diffs {
  ($lt: lifetime) => { impl Iterator<Item = Diff<$lt>> };
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Position {
  line: usize,
  column: usize,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Range {
  /// inclusive start, exclusive end
  byte_offset: std::ops::Range<usize>,
  start: Position,
  end: Position,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LabelJSON<'a> {
  text: &'a str,
  range: Range,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchNode<'a> {
  text: Cow<'a, str>,
  range: Range,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchJSON<'a> {
  text: Cow<'a, str>,
  range: Range,
  file: Cow<'a, str>,
  #[serde(skip_serializing_if = "Option::is_none")]
  replacement: Option<Cow<'a, str>>,
  language: SupportLang,
  #[serde(skip_serializing_if = "Option::is_none")]
  meta_variables: Option<MetaVariables<'a>>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetaVariables<'a> {
  single: HashMap<String, MatchNode<'a>>,
  multi: HashMap<String, Vec<MatchNode<'a>>>,
}
fn from_env<'a>(nm: &NodeMatch<'a, SupportLang>) -> Option<MetaVariables<'a>> {
  let env = nm.get_env();
  let mut vars = env.get_matched_variables().peekable();
  vars.peek()?;
  let mut single = HashMap::new();
  let mut multi = HashMap::new();
  for var in vars {
    use MetaVariable as MV;
    match var {
      MV::Named(n) => {
        let node = env.get_match(&n).expect("must exist!");
        single.insert(
          n,
          MatchNode {
            text: node.text(),
            range: get_range(node),
          },
        );
      }
      MV::NamedEllipsis(n) => {
        let nodes = env.get_multiple_matches(&n);
        multi.insert(
          n,
          nodes
            .into_iter()
            .map(|node| MatchNode {
              text: node.text(),
              range: get_range(&node),
            })
            .collect(),
        );
      }
      _ => continue,
    }
  }
  Some(MetaVariables { single, multi })
}

fn get_range(n: &Node<'_, SupportLang>) -> Range {
  let start_pos = n.start_pos();
  let end_pos = n.end_pos();
  Range {
    byte_offset: n.range(),
    start: Position {
      line: start_pos.0,
      column: start_pos.1,
    },
    end: Position {
      line: end_pos.0,
      column: end_pos.1,
    },
  }
}

impl<'a> MatchJSON<'a> {
  fn new(nm: NodeMatch<'a, SupportLang>, path: &'a str) -> Self {
    MatchJSON {
      file: Cow::Borrowed(path),
      text: nm.text(),
      language: *nm.lang(),
      replacement: None,
      range: get_range(&nm),
      meta_variables: from_env(&nm),
    }
  }
}
fn get_labels<'a>(nm: &NodeMatch<'a, SupportLang>) -> Option<Vec<MatchNode<'a>>> {
  let env = nm.get_env();
  let labels = env.get_labels("secondary")?;
  Some(
    labels
      .iter()
      .map(|l| MatchNode {
        text: l.text(),
        range: get_range(l),
      })
      .collect(),
  )
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleMatchJSON<'a> {
  #[serde(flatten)]
  matched: MatchJSON<'a>,
  rule_id: &'a str,
  severity: Severity,
  message: String,
  #[serde(skip_serializing_if = "Option::is_none")]
  labels: Option<Vec<MatchNode<'a>>>,
}
impl<'a> RuleMatchJSON<'a> {
  fn new(nm: NodeMatch<'a, SupportLang>, path: &'a str, rule: &'a RuleConfig<SupportLang>) -> Self {
    let message = rule.get_message(&nm);
    let labels = get_labels(&nm);
    let matched = MatchJSON::new(nm, path);
    Self {
      matched,
      rule_id: &rule.id,
      severity: rule.severity.clone(),
      message,
      labels,
    }
  }
}

pub struct JSONPrinter<W: Write> {
  output: Mutex<W>,
  // indicate if any matches happened
  matched: AtomicBool,
}
impl JSONPrinter<Stdout> {
  pub fn stdout() -> Self {
    Self::new(std::io::stdout())
  }
}

impl<W: Write> JSONPrinter<W> {
  pub fn new(output: W) -> Self {
    // no match happened yet
    Self {
      output: Mutex::new(output),
      matched: AtomicBool::new(false),
    }
  }
}

// TODO: refactor this shitty code.
impl<W: Write> Printer for JSONPrinter<W> {
  fn print_rule<'a>(
    &self,
    matches: Matches!('a),
    file: SimpleFile<Cow<str>, &String>,
    rule: &RuleConfig<SupportLang>,
  ) {
    let mut matches = matches.peekable();
    if matches.peek().is_none() {
      return;
    }
    let mut lock = self.output.lock().expect("should work");
    let matched = self.matched.swap(true, Ordering::AcqRel);
    let path = file.name();
    if !matched {
      writeln!(&mut lock, "[").unwrap();
      let nm = matches.next().unwrap();
      let v = RuleMatchJSON::new(nm, path, rule);
      serde_json::to_writer_pretty(&mut *lock, &v).unwrap();
    }
    for nm in matches {
      writeln!(&mut lock, ",").unwrap();
      let v = RuleMatchJSON::new(nm, path, rule);
      serde_json::to_writer_pretty(&mut *lock, &v).unwrap();
    }
  }

  fn print_matches<'a>(&self, matches: Matches!('a), path: &Path) -> Result<()> {
    let mut matches = matches.peekable();
    if matches.peek().is_none() {
      return Ok(());
    }
    let mut lock = self.output.lock().expect("should work");
    let matched = self.matched.swap(true, Ordering::AcqRel);
    let path = path.to_string_lossy();
    if !matched {
      writeln!(lock, "[").unwrap();
      let nm = matches.next().unwrap();
      let v = MatchJSON::new(nm, &path);
      serde_json::to_writer_pretty(&mut *lock, &v).unwrap();
    }
    for nm in matches {
      writeln!(lock, ",").unwrap();
      let v = MatchJSON::new(nm, &path);
      serde_json::to_writer_pretty(&mut *lock, &v).unwrap();
    }
    Ok(())
  }

  fn print_diffs<'a>(&self, diffs: Diffs!('a), path: &Path) -> Result<()> {
    let mut diffs = diffs.peekable();
    if diffs.peek().is_none() {
      return Ok(());
    }
    let mut lock = self.output.lock().expect("should work");
    let matched = self.matched.swap(true, Ordering::AcqRel);
    let path = path.to_string_lossy();
    if !matched {
      writeln!(lock, "[").unwrap();
      let diff = diffs.next().unwrap();
      let mut v = MatchJSON::new(diff.node_match, &path);
      v.replacement = Some(diff.replacement);
      serde_json::to_writer_pretty(&mut *lock, &v).unwrap();
    }
    for diff in diffs {
      writeln!(lock, ",").unwrap();
      let mut v = MatchJSON::new(diff.node_match, &path);
      v.replacement = Some(diff.replacement);
      serde_json::to_writer_pretty(&mut *lock, &v).unwrap();
    }
    Ok(())
  }
  fn print_rule_diffs<'a>(
    &self,
    diffs: Diffs!('a),
    path: &Path,
    _rule: &RuleConfig<SupportLang>,
  ) -> Result<()> {
    self.print_diffs(diffs, path)
  }

  fn after_print(&self) {
    let matched = self.matched.load(Ordering::Acquire);
    if matched {
      println!();
    } else {
      print!("[");
    }
    println!("]");
  }
}

#[cfg(test)]
mod test {
  #[test]
  #[ignore]
  fn test_invariant() {}
}