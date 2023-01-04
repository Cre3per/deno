// Copyright 2018-2023 the Deno authors. All rights reserved. MIT license.

use crate::errors::get_error_class_name;
use crate::file_fetcher::FileFetcher;
use crate::npm;

use deno_core::futures;
use deno_core::futures::FutureExt;
use deno_core::ModuleSpecifier;
use deno_graph::source::CacheInfo;
use deno_graph::source::LoadFuture;
use deno_graph::source::LoadResponse;
use deno_graph::source::Loader;
use deno_runtime::permissions::Permissions;
use std::collections::HashMap;
use std::sync::Arc;

mod check;
mod common;
mod deno_dir;
mod disk_cache;
mod emit;
mod http_cache;
mod incremental;
mod node;
mod parsed_source;

pub use check::TypeCheckCache;
pub use common::FastInsecureHasher;
pub use deno_dir::DenoDir;
pub use disk_cache::DiskCache;
pub use emit::EmitCache;
pub use http_cache::CachedUrlMetadata;
pub use http_cache::HttpCache;
pub use incremental::IncrementalCache;
pub use node::NodeAnalysisCache;
pub use parsed_source::ParsedSourceCache;

/// Permissions used to save a file in the disk caches.
pub const CACHE_PERM: u32 = 0o644;

/// A "wrapper" for the FileFetcher and DiskCache for the Deno CLI that provides
/// a concise interface to the DENO_DIR when building module graphs.
pub struct FetchCacher {
  emit_cache: EmitCache,
  dynamic_permissions: Permissions,
  file_fetcher: Arc<FileFetcher>,
  file_header_overrides: HashMap<ModuleSpecifier, HashMap<String, String>>,
  root_permissions: Permissions,
}

impl FetchCacher {
  pub fn new(
    emit_cache: EmitCache,
    file_fetcher: FileFetcher,
    file_header_overrides: HashMap<ModuleSpecifier, HashMap<String, String>>,
    root_permissions: Permissions,
    dynamic_permissions: Permissions,
  ) -> Self {
    let file_fetcher = Arc::new(file_fetcher);

    Self {
      emit_cache,
      dynamic_permissions,
      file_fetcher,
      file_header_overrides,
      root_permissions,
    }
  }
}

impl Loader for FetchCacher {
  fn get_cache_info(&self, specifier: &ModuleSpecifier) -> Option<CacheInfo> {
    if specifier.scheme() == "npm" {
      return None;
    }

    let local = self.file_fetcher.get_local_path(specifier)?;
    if local.is_file() {
      let emit = self
        .emit_cache
        .get_emit_filepath(specifier)
        .filter(|p| p.is_file());
      Some(CacheInfo {
        local: Some(local),
        emit,
        map: None,
      })
    } else {
      None
    }
  }

  fn load(
    &mut self,
    specifier: &ModuleSpecifier,
    is_dynamic: bool,
  ) -> LoadFuture {
    fn maybe_extend_optional_map(
      maybe_map: Option<&HashMap<String, String>>,
      maybe_extend: Option<&HashMap<String, String>>,
    ) -> Option<HashMap<String, String>> {
      if maybe_map.is_none() && maybe_extend.is_none() {
        None
      } else {
        let mut headers = HashMap::<String, String>::new();
        if let Some(map) = maybe_map {
          headers.extend(map.clone());
        }
        if let Some(extend) = maybe_extend {
          headers.extend(extend.clone());
        }
        Some(headers)
      }
    }

    if specifier.scheme() == "npm" {
      return Box::pin(futures::future::ready(
        match npm::NpmPackageReference::from_specifier(specifier) {
          Ok(_) => Ok(Some(deno_graph::source::LoadResponse::External {
            specifier: specifier.clone(),
          })),
          Err(err) => Err(err),
        },
      ));
    }

    let specifier = specifier.clone();
    let mut permissions = if is_dynamic {
      self.dynamic_permissions.clone()
    } else {
      self.root_permissions.clone()
    };
    let file_fetcher = self.file_fetcher.clone();
    let file_header_overrides = self.file_header_overrides.clone();

    async move {
      file_fetcher
        .fetch(&specifier, &mut permissions)
        .await
        .map_or_else(
          |err| {
            if let Some(err) = err.downcast_ref::<std::io::Error>() {
              if err.kind() == std::io::ErrorKind::NotFound {
                return Ok(None);
              }
            } else if get_error_class_name(&err) == "NotFound" {
              return Ok(None);
            }
            Err(err)
          },
          |file| {
            let maybe_overridden_headers =
              file_header_overrides.get(&specifier);

            let maybe_headers = maybe_extend_optional_map(
              file.maybe_headers.as_ref(),
              maybe_overridden_headers,
            );

            Ok(Some(LoadResponse::Module {
              specifier: file.specifier,
              maybe_headers,
              content: file.source,
            }))
          },
        )
    }
    .boxed()
  }
}
