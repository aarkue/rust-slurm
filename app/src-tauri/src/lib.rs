use std::{
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufReader, BufWriter},
    path::PathBuf,
};

use anyhow::Error;
use chrono::{DateTime, FixedOffset, NaiveDateTime, Utc};
use glob::glob;
use process_mining::{
    export_ocel_json_path,
    ocel::ocel_struct::{
        OCELAttributeType, OCELEvent, OCELObject, OCELObjectAttribute, OCELRelationship, OCELType,
        OCELTypeAttribute,
    },
    OCEL,
};
use rust_slurm::{
    self, get_squeue_res, login_with_cfg, Client, ConnectionConfig, SqueueRow,
};
use serde::Serialize;
use serde_json::map::Keys;
use structdiff::StructDiff;
use tauri::{async_runtime::Mutex, State};

#[tauri::command]
async fn run_squeue<'a>(state: State<'a, Mutex<AppState>>) -> Result<String, CmdError> {
    if let Some(client) = &state.lock().await.client {
        let (time, jobs) = get_squeue_res(&client).await?;
        serde_json::to_writer_pretty(
            BufWriter::new(
                File::create(format!("{}.json", time.to_rfc3339().replace(":", "-"))).unwrap(),
            ),
            &jobs,
        )
        .unwrap();
        Ok(format!("Got {} jobs at {}.", jobs.len(), time.to_rfc3339()))
    } else {
        Err(Error::msg("No logged-in client available.").into())
    }
}

#[tauri::command]
async fn get_squeue<'a>(
    state: State<'a, Mutex<AppState>>,
) -> Result<(DateTime<Utc>, Vec<SqueueRow>), CmdError> {
    if let Some(client) = &state.lock().await.client {
        let (time, jobs) = get_squeue_res(&client).await?;
        Ok((time, jobs))
    } else {
        Err(Error::msg("No logged-in client available.").into())
    }
}

#[tauri::command]
async fn login<'a>(
    state: State<'a, Mutex<AppState>>,
    cfg: ConnectionConfig,
) -> Result<String, CmdError> {
    let client = login_with_cfg(&cfg).await?;
    state.lock().await.client = Some(client);
    Ok(String::from("OK"))
}

#[tauri::command]
async fn logout<'a>(state: State<'a, Mutex<AppState>>) -> Result<String, CmdError> {
    if let Some(client) = state.lock().await.client.take() {
        if let Err(e) = client.disconnect().await {
            return Err(Error::from(e).into());
        }
    }
    Ok(String::from("OK"))
}

#[tauri::command]
async fn extract_ocel(
    data: Vec<(DateTime<FixedOffset>, Vec<SqueueRow>)>,
) -> Result<String, CmdError> {
    let count: usize = data.iter().map(|(_, rows)| rows.len()).sum();
    let mut ocel: OCEL = OCEL {
        event_types: Vec::new(),
        object_types: Vec::new(),
        events: Vec::new(),
        objects: Vec::new(),
    };
    #[derive(Debug, Hash, PartialEq, Eq)]
    struct JobInfo<'a> {
        pub id: &'a String,
        pub command: &'a str,
        pub work_dir: String,
        pub cpus: usize,
        pub min_memory: &'a String,
        pub submit_time: &'a NaiveDateTime,
        pub start_time: &'a Option<NaiveDateTime>,
    }
    impl<'a> From<&'a SqueueRow> for JobInfo<'a> {
        fn from(r: &'a SqueueRow) -> Self {
            Self {
                id: &r.job_id,
                command: r.command.split("/").last().unwrap_or_default(),
                work_dir: r.work_dir.to_string_lossy().to_string(),
                cpus: r.cpus,
                min_memory: &r.min_memory,
                submit_time: &r.submit_time,
                start_time: &r.start_time,
            }
        }
    }
    ocel.object_types.push(OCELType {
        name: "Job".to_string(),
        attributes: vec![
            OCELTypeAttribute::new("command", &OCELAttributeType::String),
            OCELTypeAttribute::new("work_dir", &OCELAttributeType::String),
            OCELTypeAttribute::new("cpus", &OCELAttributeType::Integer),
            OCELTypeAttribute::new("min_memory", &OCELAttributeType::String),
        ],
    });
    ocel.object_types.push(OCELType {
        name: "Account".to_string(),
        attributes: vec![],
    });
    ocel.object_types.push(OCELType {
        name: "Group".to_string(),
        attributes: vec![],
    });
    ocel.object_types.push(OCELType {
        name: "Host".to_string(),
        attributes: vec![],
    });
    ocel.object_types.push(OCELType {
        name: "Partition".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Start Job".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Submit Job".to_string(),
        attributes: vec![],
    });

    let job_ids: HashSet<_> = data
        .iter()
        .flat_map(|(_, rs)| rs)
        .map(|r| &r.job_id)
        .collect();
    let rows_per_job: HashMap<_, _> = job_ids
        .into_iter()
        .map(|j_id| {
            let mut rows = data
                .iter()
                .filter_map(|(t, rs)| {
                    rs.iter()
                        .find(|r| r.job_id == *j_id)
                        .and_then(|r| Some((t, r.clone())))
                })
                .collect::<Vec<_>>();
            rows.sort_by_key(|(t, _)| **t);

            (j_id.clone(), rows)
        })
        .collect();
    let mut jobs: HashMap<String, OCELObject> = rows_per_job
        .iter()
        .map(|(j_id, rows)| {
            let (_last_t, last_r) = rows.last().unwrap();
            ocel.events.push(OCELEvent::new(
                format!("submit_job_{}", j_id),
                "Submit Job",
                last_r.submit_time.and_utc(),
                Vec::new(),
                vec![OCELRelationship::new(j_id, "job")],
            ));
            if let Some(x) = last_r.start_time {
                ocel.events.push(OCELEvent::new(
                    format!("start_job_{}", j_id),
                    "Start Job",
                    x.and_utc(),
                    Vec::new(),
                    vec![OCELRelationship::new(j_id, "job")],
                ));
            }

            let mut o = OCELObject {
                id: j_id.clone(),
                object_type: "Job".to_string(),
                attributes: vec![
                    OCELObjectAttribute::new(
                        "command",
                        last_r.command.split("/").last().unwrap_or_default(),
                        DateTime::UNIX_EPOCH,
                    ),
                    OCELObjectAttribute::new(
                        "work_dir",
                        last_r.work_dir.to_string_lossy().to_string(),
                        DateTime::UNIX_EPOCH,
                    ),
                    OCELObjectAttribute::new("cpus", last_r.cpus, DateTime::UNIX_EPOCH),
                    OCELObjectAttribute::new(
                        "min_memory",
                        &last_r.min_memory,
                        DateTime::UNIX_EPOCH,
                    ),
                ],
                relationships: vec![
                    OCELRelationship::new(&last_r.account, "submitted by"),
                    OCELRelationship::new(&last_r.group, "submitted by group"),
                    OCELRelationship::new(&last_r.partition, "submitted on"),
                ],
            };

            if let Some(exec_host) = &last_r.exec_host {
                o.relationships
                    .push(OCELRelationship::new(exec_host, "runs on"))
            }
            (j_id.clone(), o)
        })
        .collect();

    let account_ids: HashSet<_> = data
        .iter()
        .flat_map(|(_, rs)| rs)
        .map(|r| &r.account)
        .collect();
    let accounts: HashMap<String, OCELObject> = account_ids
        .into_iter()
        .map(|a| {
            (
                a.clone(),
                OCELObject {
                    id: a.clone(),
                    object_type: "Account".to_string(),
                    attributes: Vec::default(),
                    relationships: Vec::default(),
                },
            )
        })
        .collect();

    let group_ids: HashSet<_> = data
        .iter()
        .flat_map(|(_, rs)| rs)
        .map(|r| &r.group)
        .collect();
    let groups: HashMap<String, OCELObject> = group_ids
        .into_iter()
        .map(|a| {
            (
                a.clone(),
                OCELObject {
                    id: a.clone(),
                    object_type: "Group".to_string(),
                    attributes: Vec::default(),
                    relationships: Vec::default(),
                },
            )
        })
        .collect();

    let exec_hosts_ids: HashSet<_> = data
        .iter()
        .flat_map(|(_, rs)| rs)
        .filter_map(|r| r.exec_host.as_ref())
        .collect();
    let exec_hosts: HashMap<String, OCELObject> = exec_hosts_ids
        .into_iter()
        .map(|a| {
            (
                a.clone(),
                OCELObject {
                    id: a.clone(),
                    object_type: "Host".to_string(),
                    attributes: Vec::default(),
                    relationships: Vec::default(),
                },
            )
        })
        .collect();

    let partition_ids: HashSet<_> = data
        .iter()
        .flat_map(|(_, rs)| rs)
        .map(|r| &r.partition)
        .collect();
    let partitions: HashMap<String, OCELObject> = partition_ids
        .into_iter()
        .map(|a| {
            (
                a.clone(),
                OCELObject {
                    id: a.clone(),
                    object_type: "Partition".to_string(),
                    attributes: Vec::default(),
                    relationships: Vec::default(),
                },
            )
        })
        .collect();

    ocel.objects.extend(jobs.into_values());
    ocel.objects.extend(accounts.into_values());
    ocel.objects.extend(exec_hosts.into_values());
    ocel.objects.extend(groups.into_values());
    ocel.objects.extend(partitions.into_values());

    // Check that all IDs are unique
    let obj_ids: HashSet<_> = ocel.objects.iter().map(|o| &o.id).collect();
    let ev_ids: HashSet<_> = ocel.events.iter().map(|e| &e.id).collect();
    assert_eq!(obj_ids.len(), ocel.objects.len());
    assert_eq!(ev_ids.len(), ocel.events.len());

    export_ocel_json_path(&ocel, "ocel-export.json").unwrap();
    Ok(format!("Got {} rows.", count))
}

struct CmdError {
    pub error: Error,
}

impl From<Error> for CmdError {
    fn from(error: Error) -> Self {
        Self { error }
    }
}

impl Serialize for CmdError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.error.to_string().as_ref())
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(Mutex::new(AppState::default()))
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![
            run_squeue,
            extract_ocel,
            login,
            logout,
            get_squeue
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[derive(Debug, Default)]
struct AppState {
    pub client: Option<Client>,
}

#[test]
fn extract_oced_delta() {
    let path =
        PathBuf::from("/home/aarkue/doc/projects/rust-slurm/crates/rust_slurm/test_squeue_loop/");

    let mut ocel: OCEL = OCEL {
        event_types: Vec::new(),
        object_types: Vec::new(),
        events: Vec::new(),
        objects: Vec::new(),
    };
    ocel.object_types.push(OCELType {
        name: "Job".to_string(),
        attributes: vec![
            OCELTypeAttribute::new("state", &OCELAttributeType::String),
            OCELTypeAttribute::new("command", &OCELAttributeType::String),
            OCELTypeAttribute::new("work_dir", &OCELAttributeType::String),
            OCELTypeAttribute::new("cpus", &OCELAttributeType::Integer),
            OCELTypeAttribute::new("min_memory", &OCELAttributeType::String),
        ],
    });

    ocel.object_types.push(OCELType {
        name: "Account".to_string(),
        attributes: vec![],
    });
    ocel.object_types.push(OCELType {
        name: "Group".to_string(),
        attributes: vec![],
    });
    ocel.object_types.push(OCELType {
        name: "Host".to_string(),
        attributes: vec![],
    });
    ocel.object_types.push(OCELType {
        name: "Partition".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Submit Job".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Job Started".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Job Ending".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Job Completed".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Job Cancelled".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Job Failed".to_string(),
        attributes: vec![OCELTypeAttribute::new("reason", &OCELAttributeType::String)],
    });

    ocel.event_types.push(OCELType {
        name: "Job Timeout".to_string(),
        attributes: vec![],
    });

    ocel.event_types.push(OCELType {
        name: "Job Out Of Memory".to_string(),
        attributes: vec![],
    });

    let mut jobs_per_time: HashMap<DateTime<Utc>, HashSet<String>> = HashMap::new();
    for entry in glob(&path.join("*.json").to_string_lossy()).expect("Glob failed") {
        match entry {
            Ok(j) => {
                let job_ids: HashSet<String> =
                    serde_json::from_reader(File::open(&j).unwrap()).unwrap();
                let time = extract_timestamp(
                    &j.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .replace(".json", ""),
                );
                jobs_per_time.insert(time, job_ids);
            }
            Err(_) => todo!(),
        }
    }
    let all_jobs_ids: HashSet<&String> = jobs_per_time.values().flatten().collect();
    println!("Recorded {} jobs overall.", all_jobs_ids.len());

    let mut accounts: HashSet<String> = HashSet::new();
    let mut groups: HashSet<String> = HashSet::new();
    let mut partitions: HashSet<String> = HashSet::new();
    let mut execution_hosts: HashSet<String> = HashSet::new();
    // Go through all jobs
    // Only consider jobs which start as 'PENDING'
    for job_id in all_jobs_ids {
        let mut g = glob(&path.join(job_id).join("*.json").to_string_lossy()).expect("Glob failed");
        let mut start_ev: Option<OCELEvent> = None;
        if let Some(Ok(d)) = g.next() {
            let dt = extract_timestamp(
                &d.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .replace(".json", ""),
            );
            // Initial Job Data
            // This is assumed to then be the first result (i.e., initial job data)
            let mut row: SqueueRow = serde_json::from_reader(File::open(&d).unwrap())
                .inspect_err(|e| eprintln!("Failed to deser.: {d:?}"))
                .unwrap();

            accounts.insert(row.account.clone());
            groups.insert(row.group.clone());
            partitions.insert(row.partition.clone());
            if let Some(h) = &row.exec_host {
                execution_hosts.insert(h.clone());
            }

            let mut o = OCELObject {
                id: row.job_id.clone(),
                object_type: "Job".to_string(),
                attributes: vec![
                    OCELObjectAttribute::new(
                        "command",
                        row.command.split("/").last().unwrap_or_default(),
                        DateTime::UNIX_EPOCH,
                    ),
                    OCELObjectAttribute::new(
                        "work_dir",
                        row.work_dir.to_string_lossy().to_string(),
                        DateTime::UNIX_EPOCH,
                    ),
                    OCELObjectAttribute::new("cpus", row.cpus, DateTime::UNIX_EPOCH),
                    OCELObjectAttribute::new("min_memory", &row.min_memory, DateTime::UNIX_EPOCH),
                    OCELObjectAttribute::new("state", format!("{:?}", &row.state), dt.clone()),
                ],
                relationships: vec![
                    OCELRelationship::new(format!("acc_{}", &row.account), "submitted by"),
                    OCELRelationship::new(format!("group_{}", &row.group), "submitted by group"),
                    OCELRelationship::new(format!("part_{}", &row.partition), "submitted on"),
                ],
            };
            if let Some(exec_host) = &row.exec_host {
                o.relationships.push(OCELRelationship::new(
                    format!("host_{exec_host}"),
                    "executed on",
                ));
                execution_hosts.insert(exec_host.clone());
            }

            let e = OCELEvent::new(
                format!("submit-{}-{}", o.id, ocel.events.len()),
                "Submit Job",
                row.submit_time.and_utc(),
                Vec::new(),
                vec![OCELRelationship::new(&o.id, "job"),OCELRelationship::new(format!("acc_{}", &row.account), "submitter")],
            );
            ocel.events.push(e);

            if row.state != JobState::PENDING {
                if let Some(st) = &row.start_time {
                    let e = OCELEvent::new(
                        format!("start-{}-{}", o.id, ocel.events.len()),
                        "Job Started",
                        st.and_utc(),
                        Vec::new(),
                        vec![OCELRelationship::new(&o.id, "job")],
                    );
                    start_ev = Some(e);
                }
            }
            let mut last_dt = dt;
            for d in g.flatten() {
                let dt = extract_timestamp(
                    &d.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .replace("DELTA-", "")
                        .replace(".json", ""),
                );
                if last_dt > dt {
                    eprintln!("Going backwards in time! {} {last_dt} -> {dt}", o.id);
                }

                last_dt = dt.clone();
                type D = <SqueueRow as StructDiff>::Diff;
                let delta: Vec<D> = serde_json::from_reader(File::open(&d).unwrap()).unwrap();
                row.apply_mut(delta.clone());
                for df in delta {
                    // println!("{:?}", df);
                    match df {
                        D::command(c) => {}
                        D::work_dir(w) => {}
                        D::min_memory(m) => {}
                        D::exec_host(h) => {
                            if let Some(h) = &h {
                                execution_hosts.insert(h.clone());
                            }
                        }

                        D::account(a) => {
                            accounts.insert(a.clone());
                        }
                        D::state(s) => {
                            // State update => Event!
                            let mut e = OCELEvent::new(
                                format!("{}-{}", o.id, ocel.events.len()),
                                "Submit Job",
                                dt,
                                Vec::new(),
                                vec![OCELRelationship::new(&o.id, "job")],
                            );
                            let mut ignore = false;
                            match s {
                                rust_slurm::JobState::RUNNING => {
                                    e.id = format!("{}_{}", "start-", e.id);
                                    e.event_type = "Job Started".to_string();
                                    ignore = true;
                                }
                                rust_slurm::JobState::COMPLETING => {
                                    e.id = format!("{}_{}", "ending-", e.id);
                                    e.event_type = "Job Ending".to_string()
                                }
                                rust_slurm::JobState::COMPLETED => {
                                    e.id = format!("{}_{}", "ended-", e.id);
                                    e.event_type = "Job Completed".to_string()
                                }
                                rust_slurm::JobState::CANCELLED => {
                                    e.id = format!("{}_{}", "cancelled-", e.id);
                                    e.event_type = "Job Cancelled".to_string()
                                }
                                rust_slurm::JobState::FAILED => {
                                    e.id = format!("{}_{}", "failed-", e.id);
                                    e.event_type = "Job Failed".to_string()
                                }
                                rust_slurm::JobState::TIMEOUT => {
                                    e.id = format!("{}_{}", "timeout-", e.id);
                                    e.event_type = "Job Timeout".to_string()
                                }
                                rust_slurm::JobState::OUT_OF_MEMORY => {
                                    e.id = format!("{}_{}", "oom-", e.id);
                                    e.event_type = "Job Out Of Memory".to_string()
                                }
                                rust_slurm::JobState::PENDING => {
                                    // Status change TO pending?
                                    // Hmm..
                                    eprintln!(
                                        "Unexpected job ID {} state change to pending. Attrs: {:?}",
                                        o.id, o.attributes
                                    );
                                    ignore = true;
                                }
                                rust_slurm::JobState::OTHER(other) => {
                                    eprintln!("Unexpected job state change to other: {}", other);
                                    ignore = true;
                                }
                            }
                            if !ignore {
                                ocel.events.push(e);
                            }
                        }
                        D::group(g) => {
                            groups.insert(g.clone());
                        }
                        D::partition(p) => {
                            partitions.insert(p.clone());
                        }
                        //   _ => {}
                        D::job_id(_) => {}
                        D::min_cpus(_) => {}
                        D::cpus(_) => {}
                        D::nodes(_) => {}
                        D::end_time(_) => {}
                        D::dependency(_) => {}
                        D::features(_) => {}
                        D::array_job_id(_) => {}
                        D::step_job_id(_) => {}
                        D::time_limit(_) => {}
                        D::name(_) => {}
                        D::priority(_) => {}
                        D::reason(_) => {}
                        D::start_time(st) => {
                            if row.state != JobState::PENDING {
                                if let Some(st) = st {
                                    if let Some(e) = start_ev.as_mut() {
                                        e.time = st.and_utc().into();
                                    }else {
                                        let e = OCELEvent::new(
                                            format!("start-{}-{}", o.id, ocel.events.len()),
                                            "Job Started",
                                            st.and_utc(),
                                            Vec::new(),
                                            vec![OCELRelationship::new(&o.id, "job")],
                                        );
                                        start_ev = Some(e);
                                    }
                                }
                            }
                        }
                        D::submit_time(_) => {}
                    };
                }
            }
            ocel.objects.push(o);
            if let Some(e) = start_ev {
                ocel.events.push(e);
            }
        }
    }

    ocel.objects.extend(accounts.iter().map(|a| OCELObject {
        id: format!("acc_{}", a),
        object_type: "Account".to_string(),
        attributes: Vec::default(),
        relationships: Vec::default(),
    }));

    ocel.objects.extend(groups.iter().map(|a| OCELObject {
        id: format!("group_{}", a),
        object_type: "Group".to_string(),
        attributes: Vec::default(),
        relationships: Vec::default(),
    }));

    ocel.objects.extend(partitions.iter().map(|a| OCELObject {
        id: format!("part_{}", a),
        object_type: "Partition".to_string(),
        attributes: Vec::default(),
        relationships: Vec::default(),
    }));

    ocel.objects
        .extend(execution_hosts.iter().map(|a| OCELObject {
            id: format!("host_{}", a),
            object_type: "Host".to_string(),
            attributes: Vec::default(),
            relationships: Vec::default(),
        }));

    export_ocel_json_path(&ocel, "NEW-ocel-export.json").unwrap();
}

pub fn extract_timestamp(s: &str) -> DateTime<Utc> {
    // 2025-01-04T00-55-04.789009695+00-00
    let (date, time) = s.split_once("T").unwrap();
    let dt_rfc = format!("{}T{}", date, time.replace("-", ":"));
    DateTime::parse_from_rfc3339(&dt_rfc).unwrap().to_utc()
}
