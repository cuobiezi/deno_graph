// Copyright 2018-2021 the Deno authors. All rights reserved. MIT license.

use crate::ast;
use crate::media_type::MediaType;
use crate::module_specifier::resolve_import;
use crate::module_specifier::ModuleSpecifier;
use crate::module_specifier::SpecifierError;
use crate::source::*;

use anyhow::Result;
use futures::stream::FuturesUnordered;
use futures::stream::StreamExt;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

#[derive(Debug)]
pub enum ModuleGraphError {
  LoadingErr(anyhow::Error),
  ParseErr(ast::Diagnostic),
  InvalidSource(ModuleSpecifier),
}

impl std::error::Error for ModuleGraphError {}

impl fmt::Display for ModuleGraphError {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let msg = match self {
      Self::LoadingErr(err) => {
        format!("An error was returned from the loader: {}", err)
      }
      Self::ParseErr(diagnostic) => {
        format!(
          "The module's source code would not be parsed: {}",
          diagnostic
        )
      }
      Self::InvalidSource(specifier) => {
        format!("The source code is invalid, as it does not match the expected hash in the lock file.\n  Specifier: {}", specifier)
      }
    };
    write!(f, "{}", msg)
  }
}

impl From<ast::Diagnostic> for ModuleGraphError {
  fn from(diagnostic: ast::Diagnostic) -> Self {
    Self::ParseErr(diagnostic)
  }
}

#[derive(Debug)]
enum ResolutionError {
  InvalidDowngrade,
  InvalidLocalImport,
  ResolverError(anyhow::Error),
  InvalidSpecifier(SpecifierError),
}

impl PartialEq for ResolutionError {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (Self::ResolverError(_), Self::ResolverError(_))
      | (Self::InvalidDowngrade, Self::InvalidDowngrade)
      | (Self::InvalidLocalImport, Self::InvalidLocalImport) => true,
      (Self::InvalidSpecifier(s), Self::InvalidSpecifier(o)) => s == o,
      _ => false,
    }
  }
}

impl Eq for ResolutionError {}

#[derive(Debug, PartialEq, Eq)]
enum Resolved {
  Specifier(ModuleSpecifier, ast::Span),
  Err(ResolutionError, ast::Span),
  None,
}

impl Default for Resolved {
  fn default() -> Self {
    Self::None
  }
}

impl Resolved {
  pub fn is_none(&self) -> bool {
    self == &Self::None
  }
}

#[derive(Debug, Default)]
pub struct Dependency {
  maybe_code: Resolved,
  maybe_type: Resolved,
  is_dynamic: bool,
}

#[derive(Debug)]
pub struct Module {
  dependencies: HashMap<String, Dependency>,
  maybe_types_dependency: Option<(String, Resolved)>,
  media_type: MediaType,
  source: String,
  specifier: ModuleSpecifier,
}

impl Module {
  pub fn new(specifier: ModuleSpecifier, source: String) -> Self {
    Self {
      dependencies: Default::default(),
      maybe_types_dependency: None,
      media_type: MediaType::Unknown,
      source,
      specifier,
    }
  }
}

#[derive(Debug)]
enum ModuleSlot {
  Module(Module),
  Err(ModuleGraphError),
  Missing,
  Pending,
}

#[derive(Debug)]
pub struct ModuleGraph {
  root: ModuleSpecifier,
  maybe_locker: Option<Arc<Mutex<dyn Locker>>>,
  modules: HashMap<ModuleSpecifier, ModuleSlot>,
  redirects: HashMap<ModuleSpecifier, ModuleSpecifier>,
}

impl ModuleGraph {
  pub fn new(
    root: ModuleSpecifier,
    maybe_locker: Option<Arc<Mutex<dyn Locker>>>,
  ) -> Self {
    Self {
      root,
      maybe_locker,
      modules: Default::default(),
      redirects: Default::default(),
    }
  }

  /// Determine if the graph sources are valid by calling the passed locker. If
  /// the integrity of all the sources passes or if there is no locker supplied
  /// the method results in an ok, otherwise returns an error which indicates
  /// the first specifier that failed the integrity check.
  pub fn lock(&self) -> Result<(), ModuleGraphError> {
    if let Some(locker) = &self.maybe_locker {
      let mut locker = locker.lock();
      for (_, module_slot) in self.modules.iter() {
        if let ModuleSlot::Module(module) = module_slot {
          if !locker.check_or_insert(&module.specifier, &module.source) {
            return Err(ModuleGraphError::InvalidSource(
              module.specifier.clone(),
            ));
          }
        }
      }
    }
    Ok(())
  }
}

pub(crate) struct Builder {
  is_dynamic_root: bool,
  graph: ModuleGraph,
  loader: Arc<Mutex<dyn Loader>>,
  maybe_resolver: Option<Arc<Mutex<dyn Resolver>>>,
  pending: FuturesUnordered<LoadFuture>,
}

impl Builder {
  pub fn new(
    root_specifier: ModuleSpecifier,
    is_dynamic_root: bool,
    loader: Arc<Mutex<dyn Loader>>,
    maybe_resolver: Option<Arc<Mutex<dyn Resolver>>>,
    maybe_locker: Option<Arc<Mutex<dyn Locker>>>,
  ) -> Self {
    Self {
      is_dynamic_root,
      graph: ModuleGraph::new(root_specifier, maybe_locker),
      loader,
      maybe_resolver,
      pending: FuturesUnordered::new(),
    }
  }

  pub async fn build(mut self) -> ModuleGraph {
    let root = self.graph.root.clone();
    self.load(&root, self.is_dynamic_root);

    loop {
      match self.pending.next().await {
        Some((specifier, Ok(Some(response)))) => {
          self.visit(specifier, response)
        }
        Some((specifier, Ok(None))) => {
          self.graph.modules.insert(specifier, ModuleSlot::Missing);
        }
        Some((specifier, Err(err))) => {
          self.graph.modules.insert(
            specifier,
            ModuleSlot::Err(ModuleGraphError::LoadingErr(err)),
          );
        }
        _ => {}
      }
      if self.pending.is_empty() {
        break;
      }
    }

    self.graph
  }

  /// Enqueue a request to load the specifier via the loader.
  fn load(&mut self, specifier: &ModuleSpecifier, is_dynamic: bool) {
    if !self.graph.modules.contains_key(specifier) {
      self
        .graph
        .modules
        .insert(specifier.clone(), ModuleSlot::Pending);
      let mut loader = self.loader.lock();
      let future = loader.load(specifier, is_dynamic);
      self.pending.push(future);
    }
  }

  /// Resolve a string specifier from a referring module, using the resolver if
  /// present, returning the resolution result.
  fn resolve(
    &mut self,
    specifier: &str,
    referrer: &ModuleSpecifier,
    range: &ast::Range,
  ) -> Resolved {
    let mut remapped = false;
    let resolved_specifier = if let Some(resolver) = &self.maybe_resolver {
      let resolver = resolver.lock();
      remapped = true;
      resolver
        .resolve(specifier, referrer)
        .map_err(ResolutionError::ResolverError)
    } else {
      resolve_import(specifier, referrer)
        .map_err(ResolutionError::InvalidSpecifier)
    };
    let span = ast::Span {
      specifier: referrer.clone(),
      range: range.clone(),
    };
    match resolved_specifier {
      Ok(specifier) => {
        let referrer_scheme = referrer.scheme();
        let specifier_scheme = specifier.scheme();
        if referrer_scheme == "https" && specifier_scheme == "http" {
          Resolved::Err(ResolutionError::InvalidDowngrade, span)
        } else if (referrer_scheme == "https" || referrer_scheme == "http")
          && !(specifier_scheme == "https" || specifier_scheme == "http")
          && !remapped
        {
          Resolved::Err(ResolutionError::InvalidLocalImport, span)
        } else {
          Resolved::Specifier(specifier, span)
        }
      }
      Err(err) => Resolved::Err(err, span),
    }
  }

  /// Resolve an dependency of a module and load the dependency if required,
  /// returning the resolved dependency.
  fn resolve_load(
    &mut self,
    specifier: &str,
    referrer: &ModuleSpecifier,
    range: &ast::Range,
    is_dynamic: bool,
  ) -> Resolved {
    let resolved_import = self.resolve(specifier, referrer, range);
    if let Resolved::Specifier(specifier, _) = &resolved_import {
      self.load(specifier, is_dynamic);
    };
    resolved_import
  }

  /// Visit a module, parsing it and resolving any dependencies.
  fn visit(
    &mut self,
    requested_specifier: ModuleSpecifier,
    response: LoadResponse,
  ) {
    let maybe_headers = response.maybe_headers;
    let specifier = response.specifier.clone();

    // If the response was redirected, then we add the module to the redirects
    if requested_specifier != specifier {
      self
        .graph
        .redirects
        .insert(requested_specifier, specifier.clone());
    }

    // Init the module and determine its media type
    let mut module = Module::new(response.specifier, response.content);
    module.media_type = if let Some(headers) = &maybe_headers {
      if let Some(content_type) = headers.get("content-type") {
        MediaType::from_content_type(&module.specifier, content_type)
      } else {
        MediaType::from(&module.specifier)
      }
    } else {
      MediaType::from(&module.specifier)
    };

    // Parse the module and start analyzing the module.
    let module_slot =
      match ast::parse(&module.specifier, &module.source, &module.media_type) {
        Ok(parsed_module) => {
          // Analyze the TypeScript triple-slash references
          for reference in parsed_module.analyze_ts_references() {
            match reference {
              ast::TypeScriptReference::Path(specifier, range) => {
                let resolved_dependency = self.resolve_load(
                  &specifier,
                  &module.specifier,
                  &range,
                  false,
                );
                let dep = module.dependencies.entry(specifier).or_default();
                dep.maybe_code = resolved_dependency;
              }
              ast::TypeScriptReference::Types(specifier, range) => {
                let resolved_dependency = self.resolve_load(
                  &specifier,
                  &module.specifier,
                  &range,
                  false,
                );
                if module.media_type == MediaType::JavaScript
                  || module.media_type == MediaType::Jsx
                {
                  module.maybe_types_dependency =
                    Some((specifier, resolved_dependency));
                } else {
                  let dep = module.dependencies.entry(specifier).or_default();
                  dep.maybe_type = resolved_dependency;
                }
              }
            }
          }

          // Analyze the X-TypeScript-Types header
          if module.maybe_types_dependency.is_none() {
            if let Some(headers) = &maybe_headers {
              if let Some(types_header) = headers.get("x-typescript-types") {
                let resolved_dependency = self.resolve_load(
                  types_header,
                  &module.specifier,
                  &ast::Range::default(),
                  false,
                );
                module.maybe_types_dependency =
                  Some((types_header.clone(), resolved_dependency));
              }
            }
          }

          // Analyze ES dependencies
          let descriptors = parsed_module.analyze_dependencies();
          for desc in descriptors {
            let dep = module
              .dependencies
              .entry(desc.specifier.to_string())
              .or_default();
            let resolved_dependency = self.resolve_load(
              &desc.specifier,
              &module.specifier,
              &parsed_module.range_from_span(&desc.specifier_span),
              desc.is_dynamic,
            );
            dep.maybe_code = resolved_dependency;
            let specifier = module.specifier.clone();
            let maybe_type = parsed_module
              .analyze_deno_types(&desc)
              .map(|(s, r)| {
                self.resolve_load(&s, &specifier, &r, desc.is_dynamic)
              })
              .unwrap_or_else(|| Resolved::None);
            if dep.maybe_type.is_none() {
              dep.maybe_type = maybe_type
            }
          }

          // Return the module as a valid module
          ModuleSlot::Module(module)
        }
        Err(diagnostic) => ModuleSlot::Err(diagnostic.into()),
      };
    self.graph.modules.insert(specifier, module_slot);
  }
}