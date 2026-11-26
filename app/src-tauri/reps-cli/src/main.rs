use engine::{activity,store::Store};
fn run()->Result<(),String>{
 let args:Vec<String>=std::env::args().skip(1).collect();
 let command=args.first().map(String::as_str).unwrap_or("help");
 if matches!(command,"help"|"--help"|"-h") {println!("RFP — reps for prompts\n\nrfp profile [--nickname NAME] [--sharing on|off] [--location CITY | --clear-location]\nrfp site [--url URL] [--upload-token-file PATH] [--read-token-file PATH]\nrfp workday --end HH:MM --warn-minutes 60\nrfp inspect [--watch]              Inspect the passive agent watcher\nrfp mode debug|workout            Switch modes; wait for restart\nrfp camera list|settings|status    Inspect cameras\nrfp camera set --device /dev/video0 --rotation 180\nrfp camera preview|stop           Display camera without workout credit\nrfp debug exercises|videos|history         List available tests\nrfp debug start --exercise squat [--state live|counting|no-pose|vision-down]\n      [--reps N | --seconds N] [--weight LB]   Start a (programmed) test set\nrfp debug next [--weight LB]       Advance one phase: code > locked > active > weight > logged > code\nrfp debug step [--value N]         Simulate progress (debug only)\nrfp debug done|stop [--all]        Complete or stop an isolated test\nrfp debug complete [--weight LB]   Finish the current set yourself (any mode; unverified)\nrfp debug video --exercise squat --file PATH\nrfp debug video-stop              Stop fixture inspection\nrfp display --window gym --visible on --fullscreen on --monitor 1\nrfp start                         Start the next workout; open the display\nrfp finish [--weight LB]           Save a detected completed set\nrfp finish --honor [--weight LB]   Attest you completed the prescribed set\nrfp cancel [--all]                Stop without credit; --all also stops previews, camera, windows\nrfp snooze --minutes 5|15|30       Snooze the next reminder\nrfp skip                          Skip this break\nrfp settings [--minutes N] [--lock-mode off]\nrfp routine [--file PATH]          Read or replace routine JSON\nrfp show | hide                   Show or hide the display\nrfp service start|stop|status|logs      Manage the background service\nrfp history|summary|status [--source local|remote]\nrfp sync                          Upload queued workouts (rate limited)\n\nAll commands accept --json. History accepts --from/--to YYYY-MM-DD, --cursor N, --limit 1..100.\nREPS_APP_HOME selects an isolated data directory.");return Ok(());}
 if ["site","profile"].contains(&command){let v=reps_cli::site::execute(&activity::app_home(),&args)?;if args.iter().any(|s|s=="--json"){println!("{v}");}else{println!("{}",serde_json::to_string_pretty(&v).unwrap());}return Ok(());}
 if command=="inspect"{return reps_cli::inspect::run(&args[1..]);}
 if ["mode","camera","debug","display","workday"].contains(&command){let v=reps_cli::commands::execute(&args)?;if args.iter().any(|s|s=="--json"){println!("{v}");}else{println!("{}",serde_json::to_string_pretty(&v).unwrap());}return Ok(());}
 if ["start","finish","cancel","snooze","skip","settings","routine","show","hide","service"].contains(&command){return control(&args);}

 let mut opts=std::collections::HashMap::new();let mut json=false;let mut i=1;
 while i<args.len(){if args[i]=="--json"{json=true;i+=1;continue;}
 if !["--source","--from","--to","--cursor","--limit"].contains(&args[i].as_str()){return Err(format!("Unknown option {}",args[i]));}
 let value=args.get(i+1).ok_or("Missing option value")?;opts.insert(args[i].as_str(),value.as_str());i+=2;}
 let source=opts.get("--source").copied().unwrap_or("local");if !["local","remote"].contains(&source){return Err("Source must be local or remote".into());}
 for key in ["--from","--to"]{if let Some(s)=opts.get(key){if chrono::NaiveDate::parse_from_str(s,"%Y-%m-%d").is_err(){return Err("Dates must use YYYY-MM-DD".into());}}}
 let cursor=opts.get("--cursor").unwrap_or(&"0").parse::<i64>().map_err(|_|"Invalid cursor")?;
 let limit=opts.get("--limit").unwrap_or(&"50").parse::<u32>().map_err(|_|"Invalid limit")?;
 if cursor<0||!(1..=100).contains(&limit){return Err("Cursor must be nonnegative; limit must be 1..100".into());}
 let home=activity::app_home();
 let value=if command=="sync" {serde_json::json!({"synced":reps_cli::sync(&home)?})}
 else if !["history","summary","status"].contains(&command){return Err("Unknown command; use reps --help".into());}
 else if source=="remote"{
 let c=reps_cli::config(&home,"remote.json")?;
 let mut path=format!("{command}?cursor={cursor}&limit={limit}");for key in ["--from","--to"]{if let Some(s)=opts.get(key){path.push_str(&format!("&{}={s}",&key[2..]));}}
 reps_cli::request(&c,"GET",&path,None)?
 }else if command=="status"{
 let store=Store::open_readonly(&home.join("reps.sqlite")).map_err(|e|e.to_string())?;
 serde_json::json!({"schemaVersion":1,"source":"local","runtime":reps_cli::control::send(&home,&reps_cli::control::Command::Snapshot).ok(),"workout":reps_cli::control::send(&home,&reps_cli::control::Command::Snapshot).ok().map(|v|v["snapshot"].clone()),"agents":activity::detect(),"sync":store.setting("sync_status","Not configured"),"snoozeUntil":store.setting("snooze_until","0"),"desktop":std::fs::read_to_string(home.join("status.json")).ok().and_then(|s|serde_json::from_str::<serde_json::Value>(&s).ok()).filter(|s|s["updatedAt"].as_f64().is_some_and(|t|chrono::Utc::now().timestamp() as f64-t<10.))})
 }else{
 let store=Store::open_readonly(&home.join("reps.sqlite")).map_err(|e|e.to_string())?;
 if command=="summary"{store.summary(opts.get("--from").copied(),opts.get("--to").copied()).map_err(|e|e.to_string())?}
 else {let mut records=store.records(opts.get("--from").copied(),opts.get("--to").copied(),cursor,limit+1,false).map_err(|e|e.to_string())?;
 let next=if records.len()>limit as usize {Some(records[limit as usize-1].sequence.to_string())}else{None};records.truncate(limit as usize);
 serde_json::json!({"schemaVersion":1,"source":"local","records":records,"nextCursor":next})}
 };
 if json {println!("{}",value);}else{println!("{}",serde_json::to_string_pretty(&value).unwrap());}Ok(())
}
fn main(){if let Err(error)=run(){eprintln!("reps: {error}");std::process::exit(1);}}

fn control(args:&[String])->Result<(),String>{
 use reps_cli::control::Command;
 let json=args.iter().any(|s|s=="--json");let command=args[0].as_str();
 if command=="service"{
  let positional:Vec<_>=args.iter().skip(1).filter(|s|s.as_str()!="--json").collect();
  if positional.len()!=1||!["start","stop","status","logs"].contains(&positional[0].as_str()){return Err("Use: rfp service start|stop|status|logs".into());}
  if std::env::var_os("REPS_APP_HOME").is_some(){return Err("Service commands manage the real installation; unset REPS_APP_HOME first".into());}
  let action=positional[0].as_str();
  if action=="logs"{let output=std::process::Command::new("journalctl").args(["--user","-u","rfp.service","--no-pager","-n","60","-o","cat"]).output().map_err(|e|e.to_string())?;if !output.status.success(){return Err(String::from_utf8_lossy(&output.stderr).into_owned());}if json{println!("{}",serde_json::json!({"logs":String::from_utf8_lossy(&output.stdout)}));}else{print!("{}",String::from_utf8_lossy(&output.stdout));}return Ok(());}

  let result=std::process::Command::new("systemctl").args(["--user",if action=="status"{"is-active"}else{action},"rfp.service"]).output().map_err(|e|e.to_string())?;
  if action!="status"&&!result.status.success(){return Err(String::from_utf8_lossy(&result.stderr).into_owned());}
  println!("{}",serde_json::json!({"service":"rfp","action":action,"ok":result.status.success(),"state":String::from_utf8_lossy(&result.stdout).trim()}));return Ok(());
 }
 let mut opts=std::collections::HashMap::new();let mut honor=false;let mut all=false;let mut i=1;
 while i<args.len(){
  match args[i].as_str(){
   "--json"=>{i+=1;continue;},
   "--honor" if command=="finish"=>{honor=true;i+=1;continue;},
   "--all" if command=="cancel"=>{all=true;i+=1;continue;},
   "--weight" if command=="finish"=>{},
   "--minutes" if command=="settings"||command=="snooze"=>{},
   "--lock-mode" if command=="settings"=>{},
   "--file" if command=="routine"=>{},
   _=>return Err(format!("Unknown option {} for {command}",args[i]))
  }
  let value=args.get(i+1).ok_or("Missing option value")?;
  if opts.insert(args[i].as_str(),value.as_str()).is_some(){return Err("Duplicate option".into());}i+=2;
 }
 let minutes=opts.get("--minutes").map(|v|v.parse::<u32>().map_err(|_|"Invalid minutes")).transpose()?;
 let request=match command{
  "start"=>Command::Start,"cancel"=>Command::Cancel{all},"skip"=>Command::Skip,"show"=>Command::Show,"hide"=>Command::Hide,
  "snooze"=>Command::Snooze{minutes:minutes.unwrap_or(15)},
  "finish"=>Command::Finish{weight:opts.get("--weight").unwrap_or(&"0").parse().map_err(|_|"Invalid weight")?,honor},
  "settings"=>Command::Settings{minutes,lock_mode:opts.get("--lock-mode").map(|v|match *v{"on"=>Ok(true),"off"=>Ok(false),_=>Err("Lock mode must be on or off")}).transpose()?},
  "routine"=>Command::Routine{json:opts.get("--file").map(|f|std::fs::read_to_string(f).map_err(|e|e.to_string())).transpose()?},
  _=>unreachable!()
 };
 let value=reps_cli::control::send(&activity::app_home(),&request)?;
 if json{println!("{value}");}else{println!("{}",serde_json::to_string_pretty(&value).unwrap());}
 Ok(())
}
