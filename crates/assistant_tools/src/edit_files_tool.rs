mod edit_action;
pub mod log;
mod replace;

use anyhow::{anyhow, Context, Result};
use assistant_tool::{ActionLog, Tool};
use collections::HashSet;
use edit_action::{EditAction, EditActionParser};
use futures::StreamExt;
use gpui::{App, AsyncApp, Entity, Task};
use language_model::{
    LanguageModelRegistry, LanguageModelRequest, LanguageModelRequestMessage, MessageContent, Role,
};
use log::{EditToolLog, EditToolRequestId};
use project::Project;
use replace::{replace_exact, replace_with_flexible_indent};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write;
use std::sync::Arc;
use util::ResultExt;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EditFilesToolInput {
    /// High-level edit instructions. These will be interpreted by a smaller
    /// model, so explain the changes you want that model to make and which
    /// file paths need changing. The description should be concise and clear.
    ///
    /// WARNING: When specifying which file paths need changing, you MUST
    /// start each path with one of the project's root directories.
    ///
    /// WARNING: NEVER include code blocks or snippets in edit instructions.
    /// Only provide natural language descriptions of the changes needed! The tool will
    /// reject any instructions that contain code blocks or snippets.
    ///
    /// The following examples assume we have two root directories in the project:
    /// - root-1
    /// - root-2
    ///
    /// <example>
    /// If you want to introduce a new quit function to kill the process, your
    /// instructions should be: "Add a new `quit` function to
    /// `root-1/src/main.rs` to kill the process".
    ///
    /// Notice how the file path starts with root-1. Without that, the path
    /// would be ambiguous and the call would fail!
    /// </example>
    ///
    /// <example>
    /// If you want to change documentation to always start with a capital
    /// letter, your instructions should be: "In `root-2/db.js`,
    /// `root-2/inMemory.js` and `root-2/sql.js`, change all the documentation
    /// to start with a capital letter".
    ///
    /// Notice how we never specify code snippets in the instructions!
    /// </example>
    pub edit_instructions: String,

    /// A user-friendly description of what changes are being made.
    /// This will be shown to the user in the UI to describe the edit operation. The screen real estate for this UI will be extremely
    /// constrained, so make the description extremely terse.
    ///
    /// <example>
    /// For fixing a broken authentication system:
    /// "Fix auth bug in login flow"
    /// </example>
    ///
    /// <example>
    /// For adding unit tests to a module:
    /// "Add tests for user profile logic"
    /// </example>
    pub display_description: String,
}

pub struct EditFilesTool;

impl Tool for EditFilesTool {
    fn name(&self) -> String {
        "edit-files".into()
    }

    fn description(&self) -> String {
        include_str!("./edit_files_tool/description.md").into()
    }

    fn input_schema(&self) -> serde_json::Value {
        let schema = schemars::schema_for!(EditFilesToolInput);
        serde_json::to_value(&schema).unwrap()
    }

    fn ui_text(&self, input: &serde_json::Value) -> String {
        match serde_json::from_value::<EditFilesToolInput>(input.clone()) {
            Ok(input) => input.display_description,
            Err(_) => "Edit files".to_string(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: serde_json::Value,
        messages: &[LanguageModelRequestMessage],
        project: Entity<Project>,
        action_log: Entity<ActionLog>,
        cx: &mut App,
    ) -> Task<Result<String>> {
        let input = match serde_json::from_value::<EditFilesToolInput>(input) {
            Ok(input) => input,
            Err(err) => return Task::ready(Err(anyhow!(err))),
        };

        match EditToolLog::try_global(cx) {
            Some(log) => {
                let req_id = log.update(cx, |log, cx| {
                    log.new_request(input.edit_instructions.clone(), cx)
                });

                let task = EditToolRequest::new(
                    input,
                    messages,
                    project,
                    action_log,
                    Some((log.clone(), req_id)),
                    cx,
                );

                cx.spawn(async move |cx| {
                    let result = task.await;

                    let str_result = match &result {
                        Ok(out) => Ok(out.clone()),
                        Err(err) => Err(err.to_string()),
                    };

                    log.update(cx, |log, cx| log.set_tool_output(req_id, str_result, cx))
                        .log_err();

                    result
                })
            }

            None => EditToolRequest::new(input, messages, project, action_log, None, cx),
        }
    }
}

struct EditToolRequest {
    parser: EditActionParser,
    output: String,
    changed_buffers: HashSet<Entity<language::Buffer>>,
    bad_searches: Vec<BadSearch>,
    project: Entity<Project>,
    action_log: Entity<ActionLog>,
    tool_log: Option<(Entity<EditToolLog>, EditToolRequestId)>,
}

#[derive(Debug)]
enum DiffResult {
    BadSearch(BadSearch),
    Diff(language::Diff),
}

#[derive(Debug)]
struct BadSearch {
    file_path: String,
    search: String,
}

impl EditToolRequest {
    fn new(
        input: EditFilesToolInput,
        messages: &[LanguageModelRequestMessage],
        project: Entity<Project>,
        action_log: Entity<ActionLog>,
        tool_log: Option<(Entity<EditToolLog>, EditToolRequestId)>,
        cx: &mut App,
    ) -> Task<Result<String>> {
        let model_registry = LanguageModelRegistry::read_global(cx);
        let Some(model) = model_registry.editor_model() else {
            return Task::ready(Err(anyhow!("No editor model configured")));
        };

        let mut messages = messages.to_vec();
        // Remove the last tool use (this run) to prevent an invalid request
        'outer: for message in messages.iter_mut().rev() {
            for (index, content) in message.content.iter().enumerate().rev() {
                match content {
                    MessageContent::ToolUse(_) => {
                        message.content.remove(index);
                        break 'outer;
                    }
                    MessageContent::ToolResult(_) => {
                        // If we find any tool results before a tool use, the request is already valid
                        break 'outer;
                    }
                    MessageContent::Text(_) | MessageContent::Image(_) => {}
                }
            }
        }

        messages.push(LanguageModelRequestMessage {
            role: Role::User,
            content: vec![
                include_str!("./edit_files_tool/edit_prompt.md").into(),
                input.edit_instructions.into(),
            ],
            cache: false,
        });

        cx.spawn(async move |cx| {
            let llm_request = LanguageModelRequest {
                messages,
                tools: vec![],
                stop: vec![],
                temperature: Some(0.0),
            };

            let stream = model.stream_completion_text(llm_request, &cx);
            let mut chunks = stream.await?;

            let mut request = Self {
                parser: EditActionParser::new(),
                // we start with the success header so we don't need to shift the output in the common case
                output: Self::SUCCESS_OUTPUT_HEADER.to_string(),
                changed_buffers: HashSet::default(),
                bad_searches: Vec::new(),
                action_log,
                project,
                tool_log,
            };

            while let Some(chunk) = chunks.stream.next().await {
                request.process_response_chunk(&chunk?, cx).await?;
            }

            request.finalize(cx).await
        })
    }

    async fn process_response_chunk(&mut self, chunk: &str, cx: &mut AsyncApp) -> Result<()> {
        let new_actions = self.parser.parse_chunk(chunk);

        if let Some((ref log, req_id)) = self.tool_log {
            log.update(cx, |log, cx| {
                log.push_editor_response_chunk(req_id, chunk, &new_actions, cx)
            })
            .log_err();
        }

        for action in new_actions {
            self.apply_action(action, cx).await?;
        }

        Ok(())
    }

    async fn apply_action(
        &mut self,
        (action, source): (EditAction, String),
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let project_path = self.project.read_with(cx, |project, cx| {
            project
                .find_project_path(action.file_path(), cx)
                .context("Path not found in project")
        })??;

        let buffer = self
            .project
            .update(cx, |project, cx| project.open_buffer(project_path, cx))?
            .await?;

        let result = match action {
            EditAction::Replace {
                old,
                new,
                file_path,
            } => {
                let snapshot = buffer.read_with(cx, |buffer, _cx| buffer.snapshot())?;

                cx.background_executor()
                    .spawn(Self::replace_diff(old, new, file_path, snapshot))
                    .await
            }
            EditAction::Write { content, .. } => Ok(DiffResult::Diff(
                buffer
                    .read_with(cx, |buffer, cx| buffer.diff(content, cx))?
                    .await,
            )),
        }?;

        match result {
            DiffResult::BadSearch(invalid_replace) => {
                self.bad_searches.push(invalid_replace);
            }
            DiffResult::Diff(diff) => {
                let _clock = buffer.update(cx, |buffer, cx| buffer.apply_diff(diff, cx))?;

                write!(&mut self.output, "\n\n{}", source)?;
                self.changed_buffers.insert(buffer);
            }
        }

        Ok(())
    }

    async fn replace_diff(
        old: String,
        new: String,
        file_path: std::path::PathBuf,
        snapshot: language::BufferSnapshot,
    ) -> Result<DiffResult> {
        let result =
            // Try to match exactly
            replace_exact(&old, &new, &snapshot)
            .await
            // If that fails, try being flexible about indentation
            .or_else(|| replace_with_flexible_indent(&old, &new, &snapshot));

        let Some(diff) = result else {
            return anyhow::Ok(DiffResult::BadSearch(BadSearch {
                search: old,
                file_path: file_path.display().to_string(),
            }));
        };

        anyhow::Ok(DiffResult::Diff(diff))
    }

    const SUCCESS_OUTPUT_HEADER: &str = "Successfully applied. Here's a list of changes:";
    const ERROR_OUTPUT_HEADER_NO_EDITS: &str = "I couldn't apply any edits!";
    const ERROR_OUTPUT_HEADER_WITH_EDITS: &str =
        "Errors occurred. First, here's a list of the edits we managed to apply:";

    async fn finalize(self, cx: &mut AsyncApp) -> Result<String> {
        let changed_buffer_count = self.changed_buffers.len();

        // Save each buffer once at the end
        for buffer in &self.changed_buffers {
            self.project
                .update(cx, |project, cx| project.save_buffer(buffer.clone(), cx))?
                .await?;
        }

        self.action_log
            .update(cx, |log, cx| log.buffer_edited(self.changed_buffers, cx))
            .log_err();

        let errors = self.parser.errors();

        if errors.is_empty() && self.bad_searches.is_empty() {
            if changed_buffer_count == 0 {
                return Err(anyhow!(
                    "The instructions didn't lead to any changes. You might need to consult the file contents first."
                ));
            }

            Ok(self.output)
        } else {
            let mut output = self.output;

            if output.is_empty() {
                output.replace_range(
                    0..Self::SUCCESS_OUTPUT_HEADER.len(),
                    Self::ERROR_OUTPUT_HEADER_NO_EDITS,
                );
            } else {
                output.replace_range(
                    0..Self::SUCCESS_OUTPUT_HEADER.len(),
                    Self::ERROR_OUTPUT_HEADER_WITH_EDITS,
                );
            }

            if !self.bad_searches.is_empty() {
                writeln!(
                    &mut output,
                    "\n\n# {} SEARCH/REPLACE block(s) failed to match:\n",
                    self.bad_searches.len()
                )?;

                for replace in self.bad_searches {
                    writeln!(
                        &mut output,
                        "## No exact match in: {}\n```\n{}\n```\n",
                        replace.file_path, replace.search,
                    )?;
                }

                write!(&mut output,
                    "The SEARCH section must exactly match an existing block of lines including all white \
                    space, comments, indentation, docstrings, etc."
                )?;
            }

            if !errors.is_empty() {
                writeln!(
                    &mut output,
                    "\n\n# {} SEARCH/REPLACE blocks failed to parse:",
                    errors.len()
                )?;

                for error in errors {
                    writeln!(&mut output, "- {}", error)?;
                }
            }

            if changed_buffer_count > 0 {
                writeln!(
                    &mut output,
                    "\n\nThe other SEARCH/REPLACE blocks were applied successfully. Do not re-send them!",
                )?;
            }

            writeln!(
                &mut output,
                "{}You can fix errors by running the tool again. You can include instructions, \
                but errors are part of the conversation so you don't need to repeat them.",
                if changed_buffer_count == 0 {
                    "\n\n"
                } else {
                    ""
                }
            )?;

            Err(anyhow!(output))
        }
    }
}
