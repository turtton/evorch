//! Read-only shell jobs in the execution panel. Output lives only for the selection.
use std::collections::{BTreeMap, VecDeque};
use workbench_protocol::{
    ids::{RunId, ThreadId},
    namespaces::execution::{ExecutionEvent, shell::*},
};

pub(crate) type JobKey = (RunId, String);
#[derive(Default, Clone)]
pub struct JobCatalog {
    pub(crate) jobs: BTreeMap<JobKey, ShellJobView>,
    terminal: VecDeque<JobKey>,
}
impl JobCatalog {
    pub fn insert(&mut self, job: ShellJobView) {
        let key = (job.run.clone(), job.job_uid.clone());
        if job.state != ShellJobState::Running && !self.terminal.contains(&key) {
            self.terminal.push_back(key.clone());
        }
        self.jobs.insert(key, job);
        while self.terminal.len() > MAX_RETAINED_SHELL_JOBS {
            self.jobs.remove(&self.terminal.pop_front().unwrap());
        }
    }
}
#[derive(Default)]
struct Tail {
    text: String,
    offset: u64,
    end: u64,
    gap: bool,
}
fn boundary(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}
impl Tail {
    fn append(&mut self, offset: u64, chunk: &str) {
        let Some(end) = offset.checked_add(chunk.len() as u64) else {
            return;
        };
        if end <= self.end {
            return;
        }
        if offset > self.end {
            self.text.clear();
            self.offset = offset;
            self.end = offset;
            self.gap = true;
        }
        let skip = boundary(chunk, (self.end - offset) as usize);
        if skip > (self.end - offset) as usize {
            self.text.clear();
            self.offset = offset + skip as u64;
            self.gap = true;
        }
        self.text.push_str(&chunk[skip..]);
        self.end = end;
        if self.text.len() > MAX_SHELL_TAIL_BYTES {
            let drop = boundary(&self.text, self.text.len() - MAX_SHELL_TAIL_BYTES);
            self.text.drain(..drop);
            self.offset += drop as u64;
            self.gap = true;
        }
    }
    fn read(&mut self, output: ShellOutput) {
        let old = std::mem::take(self);
        self.append(output.offset, &output.chunk);
        if old.end > self.end {
            self.append(old.offset, &old.text);
        }
        self.gap |= output.available_from > 0;
    }
}
#[derive(Default)]
pub(crate) struct ShellJobsPanel {
    thread: Option<ThreadId>,
    selected: Option<JobKey>,
    generation: u64,
    asked: bool,
    tail: Tail,
    status: String,
}
impl ShellJobsPanel {
    pub fn reset(&mut self) {
        self.generation += 1;
        self.asked = false;
        self.tail = Tail::default();
        self.status.clear();
    }
    pub fn event(&mut self, event: &ExecutionEvent) {
        if let ExecutionEvent::ShellJobOutput {
            run,
            job_uid,
            offset,
            chunk,
            ..
        } = event
            && self
                .selected
                .as_ref()
                .is_some_and(|key| &key.0 == run && &key.1 == job_uid)
        {
            self.tail.append(*offset, chunk);
        }
    }
    fn select(&mut self, key: JobKey) {
        if self.selected.as_ref() != Some(&key) {
            self.selected = Some(key);
            self.reset();
        }
    }
    pub fn request(
        &mut self,
        thread: Option<&ThreadId>,
        catalog: &JobCatalog,
    ) -> Option<(u64, ReadShellOutput)> {
        if self.thread.as_ref() != thread {
            self.thread = thread.cloned();
            self.selected = None;
            self.reset();
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|key| !catalog.jobs.contains_key(key))
        {
            self.selected = None;
            self.reset();
        }
        let key = self.selected.as_ref()?;
        if self.asked {
            return None;
        }
        let job = catalog.jobs.get(key)?;
        self.asked = true;
        Some((
            self.generation,
            ReadShellOutput {
                thread: job.thread.clone(),
                run: key.0.clone(),
                job_uid: key.1.clone(),
                offset: 0,
                max_bytes: MAX_SHELL_TAIL_BYTES as u32,
            },
        ))
    }
    pub fn reply(&mut self, generation: u64, result: Result<ShellOutput, String>) {
        if generation != self.generation {
            return;
        }
        match result {
            Ok(output)
                if self
                    .selected
                    .as_ref()
                    .is_some_and(|k| k.0 == output.run && k.1 == output.job_uid) =>
            {
                self.tail.read(output)
            }
            Ok(_) => (),
            Err(message) => self.status = message,
        }
    }
    pub fn render(&mut self, ui: &mut egui::Ui, thread: Option<&ThreadId>, catalog: &JobCatalog) {
        egui::CollapsingHeader::new("Shell jobs")
            .id_salt("core-shell-jobs")
            .default_open(true)
            .show(ui, |ui| {
                let jobs: Vec<_> = catalog
                    .jobs
                    .iter()
                    .filter(|(_, j)| Some(&j.thread) == thread)
                    .collect();
                if jobs.is_empty() {
                    ui.weak("No observed shell jobs");
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("shell-job-list")
                    .max_height(130.0)
                    .show(ui, |ui| {
                        for (key, job) in jobs {
                            let code = job
                                .exit_code
                                .map(|v| format!(" · exit {v}"))
                                .unwrap_or_default();
                            let label = format!(
                                "{} · {:?}{} · {}",
                                job.handle,
                                job.state,
                                code,
                                job.command.as_deref().unwrap_or("command unavailable")
                            );
                            if ui
                                .selectable_label(self.selected.as_ref() == Some(key), label)
                                .on_hover_text(format!("run {} · {}", job.run, job.job_uid))
                                .clicked()
                            {
                                self.select(key.clone());
                            }
                        }
                    });
                if self.selected.is_some() {
                    if self.tail.gap {
                        ui.weak("[Earlier output omitted / output gap]");
                    }
                    if !self.status.is_empty() {
                        ui.label(&self.status);
                    }
                    egui::ScrollArea::both()
                        .id_salt(("shell-job-output", &self.selected))
                        .max_height(200.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(egui::RichText::new(&self.tail.text).monospace())
                                    .selectable(true)
                                    .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                }
            });
    }
}

#[cfg(test)]
#[path = "shell_jobs_tests.rs"]
mod tests;
