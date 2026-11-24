import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useEffect, useState } from "react";
import "./retro.css";
import { Screen, type Variant, type Vision } from "./Screen";
import { PassivePreview } from "./PassivePreview";
import { useSnapshot } from "./useSnapshot";
type Display = {notice?:string|null;view:"screen"|"camera"|"video";previewOnly:boolean;lastError:string|null;vision?:Vision};
const variant: Variant = new URLSearchParams(window.location.search).get("window") === "gym" ? "gym" : "primary";
export default function App() {
 const snapshot=useSnapshot();
 const [mode,setMode]=useState<"debug"|"workout"|null>(null);
 const [display,setDisplay]=useState<Display>({view:"screen",previewOnly:false,lastError:null});
 const [error,setError]=useState<string|null>(null);
 useEffect(()=>{
  let active=true;let unlisten:(()=>void)|undefined;
  void listen<Display>("display-state",e=>{if(active)setDisplay(e.payload);}).then(fn=>{if(active)unlisten=fn;else fn();});
  void Promise.all([invoke<"debug"|"workout">("get_app_mode"),invoke<Display>("get_display_state")]).then(([m,d])=>{if(active){setMode(m);setDisplay(d);}}).catch(e=>{if(active)setError(String(e));});
  return()=>{active=false;unlisten?.();};
 },[]);
 if(error)return <p role="alert">RFP display unavailable: {error}</p>;
 if(!snapshot||!mode)return <p className="small">Connecting to RFP…</p>;
 return <main className="passive-app">{display.notice&&<aside className="day-warning" role="status">{display.notice}</aside>}{variant==="primary"&&display.view!=="screen"?
  <PassivePreview kind={display.view} error={display.lastError} vision={display.vision}/>:
  <Screen snapshot={snapshot} variant={variant} debug={mode==="debug"} vision={display.vision}/>}</main>;
}
