mod archive;
mod config;
mod downloadable;
mod gradle_bridge;
mod gradle_ls;
mod gradle_ls_server;
mod intellij;
mod intellij_server;
mod jdk;
mod labels;
mod language_server;
mod util;

use zed_extension_api::{
    self as zed, CodeLabel, Extension, LanguageServerId, Worktree,
    lsp::{Completion, Symbol},
    register_extension,
    serde_json::Value,
};

use crate::{
    gradle_ls_server::GradleLsServer, intellij_server::IntellijServer,
    language_server::LanguageServer,
};

struct Java {
    intellij_server: IntellijServer,
    gradle_ls_server: GradleLsServer,
}

impl Extension for Java {
    fn new() -> Self
    where
        Self: Sized,
    {
        Self {
            intellij_server: IntellijServer::new(),
            gradle_ls_server: GradleLsServer::new(),
        }
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<zed::Command> {
        match language_server_id.as_ref() {
            IntellijServer::SERVER_ID => self.intellij_server.command(language_server_id, worktree),
            GradleLsServer::SERVER_ID => {
                self.gradle_ls_server.command(language_server_id, worktree)
            }
            id => Err(format!("Unknown language server: {id}")),
        }
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Option<Value>> {
        match language_server_id.as_ref() {
            IntellijServer::SERVER_ID => self
                .intellij_server
                .initialization_options(language_server_id, worktree),
            GradleLsServer::SERVER_ID => self
                .gradle_ls_server
                .initialization_options(language_server_id, worktree),
            _ => Ok(None),
        }
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &Worktree,
    ) -> zed::Result<Option<Value>> {
        match language_server_id.as_ref() {
            IntellijServer::SERVER_ID => self
                .intellij_server
                .workspace_configuration(language_server_id, worktree),
            GradleLsServer::SERVER_ID => self
                .gradle_ls_server
                .workspace_configuration(language_server_id, worktree),
            _ => Ok(None),
        }
    }

    fn label_for_completion(
        &self,
        language_server_id: &LanguageServerId,
        completion: Completion,
    ) -> Option<CodeLabel> {
        match language_server_id.as_ref() {
            IntellijServer::SERVER_ID => self
                .intellij_server
                .label_for_completion(language_server_id, completion),
            GradleLsServer::SERVER_ID => self
                .gradle_ls_server
                .label_for_completion(language_server_id, completion),
            _ => None,
        }
    }

    fn label_for_symbol(
        &self,
        language_server_id: &LanguageServerId,
        symbol: Symbol,
    ) -> Option<CodeLabel> {
        match language_server_id.as_ref() {
            IntellijServer::SERVER_ID => self
                .intellij_server
                .label_for_symbol(language_server_id, symbol),
            GradleLsServer::SERVER_ID => self
                .gradle_ls_server
                .label_for_symbol(language_server_id, symbol),
            _ => None,
        }
    }
}

register_extension!(Java);
