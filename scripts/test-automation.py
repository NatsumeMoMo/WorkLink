"""Real-browser and MCP checks; stdlib only, uses an isolated WorkLink identity/profile."""
import argparse
import base64
import hashlib
import http.server
import json
import pathlib
import subprocess
import tempfile
import threading
import time
import uuid

PAGE = '''<!doctype html><meta charset="utf-8"><title>WorkLink fixture</title>
<main><h1>WorkLink automation fixture</h1><textarea id="prompt-textarea" aria-label="Question"></textarea>
<button data-testid="send-button" onclick="ask()">Send</button><button id="remove" onclick="this.remove()">Remove me</button>
<div id="answers"></div><div id="long"></div><div id="controls"></div></main>
<script>
let submissions=0;
document.querySelector('#long').innerText=('Unicode 中文🙂 evidence line.\\n').repeat(3000);
for(let i=0;i<80;i++){let b=document.createElement('button');b.textContent='Extra '+i;document.querySelector('#controls').append(b);}
async function ask(){submissions++;document.body.dataset.submissions=submissions;
let input=document.querySelector('textarea');let prompt=input.value;input.value='';
let stop=document.createElement('button');stop.dataset.testid='stop-button';stop.innerText='Stop';document.body.append(stop);
let answer=document.createElement('article');answer.setAttribute('data-message-author-role','assistant');document.querySelector('#answers').append(answer);
answer.innerText='Thinking';await new Promise(r=>setTimeout(r,3500));
for(let part of ['FINAL: ',prompt,' — remote result 中文🙂']){answer.textContent+=part;await new Promise(r=>setTimeout(r,500));}stop.remove();
}
</script>'''

def main():
    parser=argparse.ArgumentParser();parser.add_argument('--exe',default='target/debug/WorkLink.exe');parser.add_argument('--desktop',action='store_true');parser.add_argument('--headless-only',action='store_true');args=parser.parse_args()
    exe=str(pathlib.Path(args.exe).resolve());directory=tempfile.TemporaryDirectory(prefix='worklink-automation-');root=pathlib.Path(directory.name)
    checks=[]
    def raw(*argv, success=True):
        p=subprocess.run([exe,'--data-dir',str(root),'--json',*map(str,argv)],capture_output=True,timeout=100)
        v=json.loads(p.stdout.decode('utf-8-sig'))
        if success:assert p.returncode==0,(argv,v,p.stderr.decode(errors='replace'))
        return v
    def action(*argv, **kw):
        v=raw('browser','self','--background',*argv,**kw)
        if kw.get('success',True) and '--detach' not in argv:assert v['state']=='completed',v
        return v
    def check(name):checks.append(name);print('PASS '+name,flush=True)
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            data=PAGE.encode();self.send_response(200);self.send_header('Content-Type','text/html; charset=utf-8');self.send_header('Content-Length',str(len(data)));self.end_headers();self.wfile.write(data)
        def log_message(self,*args):pass
    server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Handler);threading.Thread(target=server.serve_forever,daemon=True).start()
    try:
        raw('init','--name','automation-test','--outbound-only')
        assert action('status')['result']['running'] is False
        tab=action('open',f'http://127.0.0.1:{server.server_port}/')['result']['tab']
        action('wait',tab,'--text','automation fixture')
        snap=action('snapshot',tab,'--max-items','4')['result'];assert len(snap['controls'])==4 and snap['next_offset']==4
        assert len(json.dumps(snap,ensure_ascii=False))<4000
        check('real Edge start, compact snapshot, pagination')
        small=action('snapshot',tab,'--max-chars','512')['result'];assert len(json.dumps(small,ensure_ascii=False,separators=(',',':')))<512
        assert small['next_offset'] is None or small['next_offset']>0
        check('512 character snapshot progresses')
        snap=action('snapshot',tab)['result'];refs={x['name']:x['ref'] for x in snap['controls']}
        action('fill',tab,refs['Question'],'draft 中文🙂')
        failed=action('ask',tab,'must not overwrite',success=False);assert 'draft_not_empty' in failed['error']['message']
        action('fill',tab,refs['Question'],'')
        action('click',tab,refs['Remove me'])
        failed=action('click',tab,refs['Remove me'],success=False);assert 'ref_expired' in failed['error']['message']
        check('trusted fill/click, preserve drafts, stale refs rejected')
        task_id=str(uuid.uuid4());start=time.monotonic()
        answered=action('ask',tab,'one unique question','--id',task_id)
        assert time.monotonic()-start>=8 and 'FINAL: one unique question' in answered['result']['text'],answered
        duplicate=action('ask',tab,'one unique question','--id',task_id);assert duplicate==answered
        conflict=action('ask',tab,'different','--id',task_id,success=False);assert 'id_conflict' in conflict['error']['message']
        text=action('text',tab,'--selector','#answers')['result'];assert text['text'].count('FINAL:')==1
        check('streaming answer waits for final text, UUID deduplication/conflict')
        text=action('text',tab,'--selector','#long')['result'];assert len(text['text'])<=3200 and text['next_offset']
        page=raw('tool','self','--background','read',text['result_id'],'--offset',str(text['next_offset']),'--limit','37');assert len(page['text'])==37
        search=raw('tool','self','--background','read',text['result_id'],'--query','中文','--limit','1');assert search['matches'] and search['next_offset']>0
        check('Unicode result pagination/search and tiny search budget')
        reply_path = root / 'full-reply.txt'
        retrieved = raw('tool', 'self', '--background', 'download', text['result_id'], reply_path)
        reply_bytes = reply_path.read_bytes()
        assert len(reply_bytes) > 48 * 1024 and len(reply_bytes.decode('utf-8')) == text['total_chars']
        assert hashlib.sha256(reply_bytes).hexdigest() == text['sha256'] == retrieved['sha256']
        assert text['source_truncated'] is False and 'data' not in retrieved
        check('complete Unicode reply downloaded across multiple chunks with verified SHA256')
        artifact=action('screenshot',tab)['result'];path=root/'screen.jpg'
        downloaded=raw('tool','self','--background','download',artifact['result_id'],path)
        assert hashlib.sha256(path.read_bytes()).hexdigest()==artifact['sha256']==downloaded['sha256'] and path.read_bytes()[:2]==b'\xff\xd8'
        check('on-demand JPEG artifact download with SHA256')
        waiting=action('wait',tab,'--text','never-appears-unique','--detach','--timeout','60');assert waiting['state'] in ('queued','running')
        tid=waiting['id']
        if not args.headless_only:raw('browser','self','status')
        still=raw('tool','self','--background','get',tid);assert still['state']=='running',still
        if not args.headless_only:
            mismatch=raw('tool','self','get',tid,success=False);assert 'task_mode_mismatch' in mismatch['error']
        raw('tool','self','--background','cancel',tid)
        for _ in range(20):
            v=raw('tool','self','--background','get',tid,'--wait-ms','1000',success=False)
            if v['state']=='cancelled':break
        assert v['state']=='cancelled',v
        check('foreground/background isolation and task cancellation')
        waiting=action('wait',tab,'--text','never-appears-unique','--detach','--timeout','60');raw('tool','self','--background','stop');time.sleep(.5)
        recovered=raw('tool','self','--background','get',waiting['id'],success=False);assert recovered['state'] in ('failed','interrupted'),recovered
        assert 'helper_restarted' in json.dumps(recovered) or 'interrupted' in json.dumps(recovered)
        check('helper restart never replays pending actions')
        requests=[{'jsonrpc':'2.0','id':1,'method':'initialize','params':{'protocolVersion':'2024-11-05'}},{'jsonrpc':'2.0','method':'notifications/initialized'},{'jsonrpc':'2.0','id':2,'method':'tools/list'}, {'jsonrpc':'2.0','id':3,'method':'tools/call','params':{'name':'worklink_browser','arguments':{'device':'self','background':True,'action':{'kind':'browser_status'},'wait_ms':2000}}}]
        p=subprocess.run([exe,'--data-dir',str(root),'mcp'],input=''.join(json.dumps(r)+'\n' for r in requests).encode(),capture_output=True,timeout=20)
        responses=[json.loads(line) for line in p.stdout.splitlines()];assert len(responses)==3 and len(responses[1]['result']['tools'])==5
        assert not responses[2]['result']['isError'] and 'structuredContent' not in responses[2]['result']
        check('MCP initialization/catalog/call and single compact response')
        if args.desktop:
            desktop_test(raw, root, check)
            rejected = raw('desktop', 'self', 'ask-chatgpt', '--prompt', 'local regression',
                           '--conversation', 'https://example.com/c/00000000-0000-4000-8000-000000000000',
                           success=False)
            assert rejected['error']['code'] == 'invalid_conversation', rejected
            assert rejected['error']['may_have_applied'] is False, rejected
            check('Edge worker receives its first request and rejects foreign conversation URLs before submission')
        print(json.dumps({'passed':len(checks),'checks':checks},ensure_ascii=False),flush=True)
    finally:
        cleanup=[('browser','self','--background','close'),('tool','self','--background','stop')]
        if not args.headless_only:cleanup.append(('tool','self','stop'))
        for argv in cleanup:
            try:raw(*argv,success=False)
            except Exception:pass
        server.shutdown()
        # Windows 上浏览器退出后，测试配置中的文件句柄可能还需要一点时间释放。
        for attempt in range(6):
            try:
                directory.cleanup()
                break
            except OSError:
                if attempt == 5:
                    raise
                time.sleep(1)

def desktop_test(raw,root,check):
    fixture='''Add-Type -AssemblyName System.Windows.Forms
$f=[Windows.Forms.Form]::new();$f.Text='WorkLink fixture UI';$f.Width=480;$f.Height=260
$t=[Windows.Forms.TextBox]::new();$t.AccessibleName='Fixture input';$t.Top=20;$t.Width=400;$f.Controls.Add($t)
$b=[Windows.Forms.Button]::new();$b.Text='Apply';$b.Top=60;$f.Controls.Add($b)
$l=[Windows.Forms.Label]::new();$l.Text='Waiting';$l.Top=110;$l.Width=420;$f.Controls.Add($l)
$b.Add_Click({$l.Text='Applied '+$t.Text});$f.Add_Shown({$f.Activate()});[void]$f.ShowDialog()'''
    encoded=base64.b64encode(fixture.encode('utf-16le')).decode()
    launched=raw('desktop','self','launch',r'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe','--','-NoProfile','-EncodedCommand',encoded)
    pid=launched['result']['pid']
    try:
        for _ in range(20):
            windows=raw('desktop','self','windows');found=[w for w in windows['result']['windows'] if w['pid']==pid]
            if found:break
            time.sleep(.3)
        assert found,windows
        hwnd=found[0]['window'];snap=raw('desktop','self','snapshot',hwnd)['result'];controls=snap['controls']
        field=next(c for c in controls if 'value' in c['patterns']);button=next(c for c in controls if c['name']=='Apply')
        raw('desktop','self','fill',field['ref'],'Remote UI 中文')
        raw('desktop','self','invoke',button['ref'])
        snap=raw('desktop','self','snapshot',hwnd)['result'];assert any('Applied Remote UI 中文' in c['name'] for c in snap['controls']),snap
        shot=raw('desktop','self','screenshot',hwnd)['result'];assert shot['mime']=='image/jpeg' and shot['bytes']>100
        raw('tool','self','download',shot['result_id'],root/'desktop.jpg')
        check('real WinForms UIA windows/snapshot/fill/invoke/screenshot/download')
    finally:
        subprocess.run(['taskkill','/PID',str(pid),'/T','/F'],capture_output=True)

if __name__=='__main__':main()
