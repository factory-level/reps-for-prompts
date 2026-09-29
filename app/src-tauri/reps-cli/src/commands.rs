use crate::control::{Command,send};
use std::collections::HashMap;
pub fn execute(args:&[String])->Result<serde_json::Value,String>{
 let group=args[0].as_str();
 if group=="mode"&&std::env::var_os("REPS_APP_HOME").is_some(){return Err("Mode switching requires the installed user service; unset REPS_APP_HOME".into());}
 let all=group=="debug"&&args.iter().any(|s|s=="--all");
 let rest:Vec<_>=args.iter().skip(1).filter(|s|s.as_str()!="--json"&&s.as_str()!="--all").collect();
 let mut opts=HashMap::new();let mut positional=Vec::new();let mut i=0;
 while i<rest.len(){if rest[i].starts_with("--"){
  let val=rest.get(i+1).ok_or("Missing option value")?;
  if opts.insert(rest[i].as_str(),val.as_str()).is_some(){return Err("Duplicate option".into());}i+=2;
 }else{positional.push(rest[i].as_str());i+=1;}}
 if positional.len()>1{return Err("Unexpected argument".into());}
 let allowed:&[&str]=match group{"workday"=>&["--end","--warn-minutes"],"mode"=>&[],"camera"=>&["--file","--device","--rotation","--phone-url","--phone-rotation","--consensus"],"debug"=>&["--exercise","--value","--file","--state","--reps","--seconds","--weight"],"display"=>&["--window","--visible","--fullscreen","--monitor"],_=>return Err("Unknown command group".into())};
 if let Some(k)=opts.keys().find(|k|!allowed.contains(k)){return Err(format!("Unknown option {k}"));}
 let boolean=|key:&str|->Result<Option<bool>,String>{opts.get(key).map(|v|match *v{"on"=>Ok(true),"off"=>Ok(false),_=>Err(format!("{key} must be on or off"))}).transpose()};
 let request=match group{
  "workday"=>{if !positional.is_empty(){return Err("Use: rfp workday --end HH:MM --warn-minutes N".into());}Command::Workday{end:opts.get("--end").map(|s|s.to_string()),warn_minutes:opts.get("--warn-minutes").map(|s|s.parse().map_err(|_|"Invalid warning minutes")).transpose()?}},
  "mode"=>Command::Mode{mode:positional.first().ok_or("Use: rfp mode debug|workout")?.to_string()},
  "display"=>{
   if !positional.is_empty(){return Err("Use display flags, without a subcommand".into());}
   Command::Display{window:opts.get("--window").unwrap_or(&"main").to_string(),visible:boolean("--visible")?,fullscreen:boolean("--fullscreen")?,monitor:opts.get("--monitor").map(|s|s.parse().map_err(|_|"Invalid monitor index")).transpose()?}
  },
  // Tired, or the detector missed it: attest the current set, in any mode.
  "debug" if positional.first()==Some(&"complete")=>Command::Finish{weight:opts.get("--weight").map(|s|s.parse().map_err(|_|"Invalid weight")).transpose()?.unwrap_or(0.),honor:true},
  "debug"=>{
   let number=|key:&str|opts.get(key).map(|s|s.parse::<f64>().map_err(|_|format!("Invalid {key}"))).transpose();
   Command::Debug{operation:positional.first().copied().unwrap_or("exercises").into(),exercise:opts.get("--exercise").map(|s|s.to_string()),value:number("--value")?,video:opts.get("--file").map(|s|std::fs::canonicalize(s).map(|p|p.to_string_lossy().into_owned()).map_err(|e|e.to_string())).transpose()?,
    state:opts.get("--state").map(|s|s.to_string()),reps:opts.get("--reps").map(|s|s.parse().map_err(|_|"Invalid --reps")).transpose()?,seconds:number("--seconds")?,weight:number("--weight")?,all}
  },
  "camera"=>{
   let operation=positional.first().copied().unwrap_or("status");
   if operation!="set"&&!opts.is_empty(){return Err("Camera options require: rfp camera set".into());}
   let mut settings=if let Some(file)=opts.get("--file"){serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(file).map_err(|e|e.to_string())?).map_err(|e|e.to_string())?}else{serde_json::json!({})};
   if !settings.is_object(){return Err("Camera config must be a JSON object".into());}
   for (flag,key) in [("--device","usbDevice"),("--phone-url","phoneUrl")]{if let Some(v)=opts.get(flag){settings[key]=serde_json::json!(v);}}
   for (flag,key) in [("--rotation","usbRotation"),("--phone-rotation","phoneRotation")]{if let Some(v)=opts.get(flag){settings[key]=serde_json::json!(v.parse::<u16>().map_err(|_|"Invalid camera rotation")?);}}
   if let Some(on)=boolean("--consensus")?{settings["consensus"]=serde_json::json!(on);}
   Command::Camera{operation:operation.into(),settings:if operation=="set"{Some(settings)}else{None}}
  },_=>unreachable!()
 };
 let home=engine::activity::app_home();let result=send(&home,&request)?;
 if let Command::Mode{mode}=request{
  if result["restartRequired"]==true{
   let restarted=std::process::Command::new("systemctl").args(["--user","restart","rfp.service"]).output().map_err(|e|e.to_string())?;
   if !restarted.status.success(){return Err("Mode was saved, but service restart failed. Run: rfp service start".into());}
   for _ in 0..60{std::thread::sleep(std::time::Duration::from_millis(250));if let Ok(state)=send(&home,&Command::Snapshot){if state["mode"]==mode{return Ok(state);}}}
   return Err("Mode was saved, but restart did not finish. Run: rfp service status".into());
  }
 }
 Ok(result)
}
