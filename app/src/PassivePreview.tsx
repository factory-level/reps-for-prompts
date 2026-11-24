import {listen} from '@tauri-apps/api/event';
import {useEffect,useState} from 'react';
import type {Vision} from './Screen';
type Frame={src:string;at:number};
type VideoEvent={event:string;jpegB64?:string;value?:number;unit?:string;total?:number;message?:string;code?:number};
export function PassivePreview({kind,error,vision}:{kind:'camera'|'video';error:string|null;vision?:Vision}){
 const [frames,setFrames]=useState<Record<string,Frame>>({}),[status,setStatus]=useState('Waiting for frames…'),[failure,setFailure]=useState(error),[now,setNow]=useState(Date.now());
 useEffect(()=>{
  let active=true;const removers:(()=>void)[]=[];
  const track=(promise:Promise<()=>void>)=>void promise.then(fn=>{if(active)removers.push(fn);else fn();}).catch(e=>{if(active)setFailure(String(e));});
  setFrames({});setFailure(error);setStatus('Waiting for frames…');
  const timer=setInterval(()=>setNow(Date.now()),1000);
  if(kind==='camera')track(listen<{cameraId:string;data:{jpegB64?:string}}>('vision-camera',({payload:p})=>{
   if(active&&p.data.jpegB64){setFrames(f=>({...f,[p.cameraId]:{src:`data:image/jpeg;base64,${p.data.jpegB64}`,at:Date.now()}}));setStatus('Live camera · local display');}
  }));
  else track(listen<VideoEvent>('debug-stream',({payload:p})=>{
   if(!active)return;
   if(p.event==='frame'&&p.jpegB64)setFrames({video:{src:`data:image/jpeg;base64,${p.jpegB64}`,at:Date.now()}});
   if(p.event==='progress')setStatus(`${p.value} ${p.unit} · debug only`);
   if(p.event==='done')setStatus(`Test finished · ${p.total} · no workout credit`);
   if(p.event==='error')setFailure(p.message||'Video test failed');
   if(p.event==='exited'&&p.code)setFailure(`Video test exited (${p.code})`);
  }));
  return()=>{active=false;clearInterval(timer);removers.forEach(fn=>fn());};
 },[kind,error]);
 // The video test runs its own process; only the camera preview depends on the hub.
 const shown=failure??(kind==='camera'&&vision?.state==='unavailable'?vision.reason??'Camera unavailable':null);
 return <section className="passive-preview"><h1>{kind==='camera'?'CAMERA':'VIDEO TEST'}</h1><div className="preview-frames">{Object.entries(frames).map(([id,f])=><figure key={id}><img src={f.src} alt={`${id} preview`}/><figcaption>{id}{kind==='camera'&&now-f.at>3000?' · no recent frames':''}</figcaption></figure>)}</div><p role="status">{status}</p>{shown?<p role="alert">{shown}</p>:null}<p className="cli-caption">{kind==='camera'?'rfp camera stop':'rfp debug video-stop'}</p></section>;
}
