'use strict';
const el=id=>document.getElementById(id);
let csrf='',saved=false,loaded=false;
async function request(path,body){const options={credentials:'same-origin',headers:{}};if(body!==undefined){options.method='POST';options.headers={'Content-Type':'application/json','X-CSRF-Token':csrf};options.body=JSON.stringify(body);}const r=await fetch(path,options);let data;try{data=await r.json();}catch{throw new Error('伺服器回應異常，請稍後重試');}if(!r.ok)throw new Error(data.error||'無法更新設定');return data;}
function render(){el('registration-open').checked=saved;el('registration-status').textContent=saved?'目前開放註冊':'目前已關閉註冊';el('registration-open').disabled=!loaded;el('save-settings').disabled=true;}
el('registration-open').addEventListener('change',()=>{el('save-settings').disabled=!loaded||el('registration-open').checked===saved;el('registration-status').textContent=el('registration-open').checked===saved?(saved?'目前開放註冊':'目前已關閉註冊'):'設定尚未儲存';el('admin-message').hidden=true;});
el('admin-form').addEventListener('submit',async e=>{e.preventDefault();if(!loaded)return;el('save-settings').disabled=true;el('registration-open').disabled=true;el('admin-error').hidden=true;el('admin-message').hidden=true;try{const data=await request('/api/admin/settings',{registration_open:el('registration-open').checked});saved=data.registration_open;render();el('admin-message').textContent='設定已儲存，即刻生效。';el('admin-message').hidden=false;}catch(e){el('admin-error').textContent=e.message;el('admin-error').hidden=false;el('registration-open').disabled=false;el('save-settings').disabled=el('registration-open').checked===saved;}});
let sessionLoaded=false,sessionSaving=false;
function sessionBusy(busy){['session-hours','save-session'].forEach(id=>el(id).disabled=busy||!sessionLoaded);}
async function loadSession(){try{const data=await request('/api/admin/session-policy');el('session-hours').value=data.hours;sessionLoaded=true;sessionBusy(false);el('session-status').textContent='設定已載入，修改後請按儲存。';}catch(e){el('session-error').textContent=e.message;el('session-error').hidden=false;el('session-status').textContent='無法讀取設定';}}
el('session-form').addEventListener('submit',async e=>{
    e.preventDefault();if(!sessionLoaded||sessionSaving)return;
    const hours=Number(el('session-hours').value);el('session-error').hidden=true;
    if(!Number.isSafeInteger(hours)||hours<1||hours>8760){el('session-error').textContent='請輸入 1–8760 的整數小時。';el('session-error').hidden=false;return;}
    sessionSaving=true;sessionBusy(true);el('session-status').textContent='儲存中…';
    try{const data=await request('/api/admin/session-policy',{hours});el('session-hours').value=data.hours;el('session-status').textContent='登入有效時間已儲存，新登入立即套用。';}
    catch(e){el('session-error').textContent=e.message;el('session-error').hidden=false;el('session-status').textContent='設定尚未儲存';}
    finally{sessionSaving=false;sessionBusy(false);}
});
let page=1,total=0,pendingFile=null,deleting=false,loadingFiles=false;
let limitsLoaded=false,limitsSaving=false;
function limitsBusy(busy){['limit-stream','limit-job','limit-storage','save-limits'].forEach(id=>el(id).disabled=busy||!limitsLoaded);}
function renderLimits(data){el('limit-stream').value=data.stream_mb;el('limit-job').value=data.job_mb;el('limit-storage').value=data.storage_mb;}
async function loadLimits(){try{const data=await request('/api/admin/download-limits');renderLimits(data);limitsLoaded=true;limitsBusy(false);el('limits-status').textContent='設定已載入，修改後請按儲存。';}catch(e){el('limits-error').textContent=e.message;el('limits-error').hidden=false;el('limits-status').textContent='無法讀取設定';}}
el('limits-form').addEventListener('submit',async e=>{
    e.preventDefault();if(!limitsLoaded||limitsSaving)return;
    const limits={stream_mb:Number(el('limit-stream').value),job_mb:Number(el('limit-job').value),storage_mb:Number(el('limit-storage').value)};
    el('limits-error').hidden=true;
    if(!Object.values(limits).every(Number.isSafeInteger)||limits.job_mb<limits.stream_mb||limits.storage_mb<3*limits.job_mb){el('limits-error').textContent='請填寫整數；處理上限不可低於串流上限，總容量須至少為處理上限的 3 倍。';el('limits-error').hidden=false;return;}
    limitsSaving=true;limitsBusy(true);el('limits-status').textContent='儲存中…';
    try{const data=await request('/api/admin/download-limits',limits);renderLimits(data);el('limits-status').textContent='大小限制已儲存，新下載立即套用。';}
    catch(e){el('limits-error').textContent=e.message;el('limits-error').hidden=false;el('limits-status').textContent='設定尚未儲存';}
    finally{limitsSaving=false;limitsBusy(false);}
});
function node(tag,cls,text){const n=document.createElement(tag);if(cls)n.className=cls;if(text!==undefined)n.textContent=text;return n;}
let retentionLoaded=false,retentionSaving=false;
function retentionBusy(busy){['retention-file-hours','retention-history-hours','save-retention'].forEach(id=>el(id).disabled=busy||!retentionLoaded);}
function renderRetentionSettings(data){el('retention-file-hours').value=data.file_hours;el('retention-history-hours').value=data.history_hours;}
async function loadRetention(){try{const data=await request('/api/admin/retention');renderRetentionSettings(data);retentionLoaded=true;retentionBusy(false);el('retention-status').textContent='設定已載入，修改後請按儲存。';}catch(e){el('retention-error').textContent=e.message;el('retention-error').hidden=false;el('retention-status').textContent='無法讀取設定';}}
el('retention-form').addEventListener('submit',async e=>{
    e.preventDefault();if(!retentionLoaded||retentionSaving)return;
    const policy={file_hours:Number(el('retention-file-hours').value),history_hours:Number(el('retention-history-hours').value)};
    el('retention-error').hidden=true;
    if(!Object.values(policy).every(Number.isSafeInteger)||policy.file_hours<1||policy.history_hours<=policy.file_hours){el('retention-error').textContent='請填寫有效整數，紀錄保留時間必須大於檔案有效時間。';el('retention-error').hidden=false;return;}
    retentionSaving=true;retentionBusy(true);el('retention-status').textContent='儲存中…';
    try{const data=await request('/api/admin/retention',policy);renderRetentionSettings(data);el('retention-status').textContent='保留時間已儲存，已套用至下載清單。';await Promise.all([loadFiles(),loadUsers()]);}
    catch(e){el('retention-error').textContent=e.message;el('retention-error').hidden=false;el('retention-status').textContent='設定尚未儲存';}
    finally{retentionSaving=false;retentionBusy(false);}
});
function pagination(){el('previous-page').disabled=loadingFiles||page<=1;el('next-page').disabled=loadingFiles||page*40>=total;el('file-page').textContent='第 '+page+' 頁';el('refresh-files').disabled=loadingFiles;el('file-user').disabled=loadingFiles;document.querySelectorAll('.view-user-files').forEach(button=>button.disabled=loadingFiles);}
async function loadFiles(){if(loadingFiles)return;loadingFiles=true;pagination();el('files-error').hidden=true;el('files-status').textContent='讀取檔案中…';try{const selected=el('file-user').value;const selectedLabel=el('file-user').selectedOptions[0]?.textContent;const q=new URLSearchParams({page:String(page)});if(selected)q.set('user_id',selected);const data=await request('/api/admin/files?'+q);total=data.total;if(page>1&&!data.files.length){page--;loadingFiles=false;return loadFiles();}el('files-count').textContent=total;const users=el('file-user');users.replaceChildren(new Option('所有使用者',''));data.users.forEach(u=>users.append(new Option(u.email+' · '+u.count+' 個檔案',String(u.id))));if(selected&&!data.users.some(u=>String(u.id)===selected))users.append(new Option(selectedLabel||'使用者 #'+selected,selected));users.value=selected;const root=el('admin-files');root.replaceChildren();data.files.forEach(f=>{const row=node('div','job admin-file'),body=node('div','job-content');body.append(node('div','job-title',f.title),node('div','job-meta',f.email));const format=f.kind==='video'?'MP4 · '+f.quality+'p':f.kind==='mp3'?'MP3 · '+f.quality+' kbps':'M4A';body.append(node('div','job-meta',format+' · '+new Date(f.created*1000).toLocaleString('zh-TW')+(f.available?' · '+(f.size_bytes/1048576).toFixed(1)+' MB':' · 檔案已無法存取')));const actions=node('div','file-actions');if(f.available){const link=node('a','button small','下載');link.href='/api/admin/files/'+encodeURIComponent(f.id)+'/file';link.setAttribute('download','');actions.append(link);}const button=node('button','button small danger','刪除');button.disabled=!f.available;button.addEventListener('click',()=>{pendingFile=f;el('delete-detail').textContent=f.title+' · '+f.email;el('delete-error').hidden=true;el('delete-dialog').showModal();});actions.append(button);row.append(body,actions);root.append(row);});el('files-status').textContent=total?'共 '+total+' 個檔案，有效時間 '+data.retention.file_hours+' 小時。':'目前沒有已完成的下載檔案。';}catch(e){el('files-error').textContent=e.message;el('files-error').hidden=false;el('files-status').textContent='無法讀取檔案';}finally{loadingFiles=false;pagination();}}
el('refresh-files').addEventListener('click',loadFiles);
el('file-user').addEventListener('change',()=>{page=1;loadFiles();});
el('previous-page').addEventListener('click',()=>{page--;loadFiles();});
el('next-page').addEventListener('click',()=>{page++;loadFiles();});
el('cancel-delete').addEventListener('click',()=>el('delete-dialog').close());
el('delete-dialog').addEventListener('cancel',e=>{if(deleting)e.preventDefault();});
el('confirm-delete').addEventListener('click',async()=>{if(!pendingFile||deleting)return;deleting=true;el('confirm-delete').disabled=true;el('cancel-delete').disabled=true;el('delete-error').hidden=true;try{await request('/api/admin/files/'+encodeURIComponent(pendingFile.id)+'/delete',{confirm:true});pendingFile=null;el('delete-dialog').close();await Promise.all([loadFiles(),loadUsers()]);}catch(e){el('delete-error').textContent=e.message;el('delete-error').hidden=false;}finally{deleting=false;el('confirm-delete').disabled=false;el('cancel-delete').disabled=false;}});
let usersPage=1,usersTotal=0,loadingUsers=false;
function usersPagination(){el('previous-users').disabled=loadingUsers||usersPage<=1;el('next-users').disabled=loadingUsers||usersPage*40>=usersTotal;el('users-page').textContent='第 '+usersPage+' 頁';el('refresh-users').disabled=loadingUsers;}
async function loadUsers(){
    if(loadingUsers)return;
    loadingUsers=true;usersPagination();el('users-error').hidden=true;el('users-status').textContent='讀取使用者中…';
    try{
        const data=await request('/api/admin/users?page='+usersPage);
        usersTotal=data.total;el('users-count').textContent=usersTotal;
        const root=el('admin-users');root.replaceChildren();
        data.users.forEach(u=>{
            const row=node('div','job admin-user'),body=node('div','job-content');
            body.append(node('div','user-email',u.email),node('div','job-meta','ID '+u.id+' · '+(u.is_admin?'管理員':'一般使用者')+' · 二階段驗證'+(u.mfa?'已啟用':'未啟用')));
            const button=node('button','button small view-user-files','查看檔案 · '+u.ready_files);
            button.disabled=loadingFiles;button.setAttribute('aria-label','查看 '+u.email+' 的下載檔案');
            button.addEventListener('click',async()=>{
                if(loadingFiles)return;
                const select=el('file-user');
                if(!Array.from(select.options).some(option=>option.value===String(u.id)))select.add(new Option(u.email,String(u.id)));
                select.value=String(u.id);page=1;await loadFiles();
                el('files-title').scrollIntoView({block:'start'});
            });
            row.append(body,button);root.append(row);
        });
        el('users-status').textContent=usersTotal?'共 '+usersTotal+' 位已註冊使用者。':'目前沒有已註冊使用者。';
    }catch(e){el('users-error').textContent=e.message;el('users-error').hidden=false;el('users-status').textContent='無法讀取使用者';}
    finally{loadingUsers=false;usersPagination();}
}
el('refresh-users').addEventListener('click',loadUsers);
el('previous-users').addEventListener('click',()=>{usersPage--;loadUsers();});
el('next-users').addEventListener('click',()=>{usersPage++;loadUsers();});
(async()=>{try{const me=await request('/api/me');if(!me.is_admin)throw new Error('此功能僅限管理員使用');csrf=me.csrf;const settings=await request('/api/admin/settings');saved=settings.registration_open;loaded=true;render();await Promise.all([loadUsers(),loadFiles(),loadLimits(),loadRetention(),loadSession()]);}catch(e){el('registration-status').textContent='無法讀取設定';el('admin-error').textContent=e.message;el('admin-error').hidden=false;el('users-status').textContent='無法讀取使用者';el('files-status').textContent='無法讀取檔案';}})();
