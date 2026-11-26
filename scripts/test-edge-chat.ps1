param([switch]$Desktop)

$ErrorActionPreference = 'Stop'
$source = Join-Path $PSScriptRoot '..\src\automation\edge-chat.ps1'
. ([ScriptBlock]::Create([IO.File]::ReadAllText($source)))

function Assert-EdgeTest([bool]$condition, [string]$message) {
    if (!$condition) { throw $message }
}

Assert-EdgeTest (Edge-ModelMatches 'GPT-6 Pro' 'Pro') 'Versioned Pro did not match.'
Assert-EdgeTest (Edge-ModelMatches '6 Thinking' 'Thinking') 'Versioned Thinking did not match.'
Assert-EdgeTest (!(Edge-ModelMatches '6 Pro' '6 Thinking')) 'A different model matched.'
Assert-EdgeTest (!(Edge-ModelMatches 'Upgrade to Pro' 'Pro')) 'An upgrade label matched Pro.'
Assert-EdgeTest ((Edge-GetUri 'chatgpt.com/c/00000000-0000-4000-8000-000000000000').Host -eq 'chatgpt.com') 'Edge shorthand URL failed.'
foreach ($address in @('http://chatgpt.com/', 'https://chatgpt.com.evil.test/', 'https://chatgpt.com:8443/', 'https://chatgpt.com@evil.test/', 'https://user@chatgpt.com/')) {
    Assert-EdgeTest (!$([bool](Edge-GetUri $address))) ('Unexpected valid URL: ' + $address)
}
Write-Output 'PASS model matching and site verification'
if (!$Desktop) { return }

$edge = Join-Path ([Environment]::GetEnvironmentVariable('ProgramFiles(x86)')) 'Microsoft\Edge\Application\msedge.exe'
$folder = Join-Path ([IO.Path]::GetTempPath()) ('WorkLink-EdgeChatTest-' + [Guid]::NewGuid().ToString('N'))
[void][IO.Directory]::CreateDirectory($folder)
$page = Join-Path $folder 'fixture.html'
$fixture = @'
<!doctype html>
<meta charset="utf-8"><title>WorkLink Edge Chat test</title>
<div role="group" aria-label="Choose ChatGPT model"><span>6 Pro</span></div>
<main id="thread"></main>
<div contenteditable="true" role="textbox" aria-label="Ask ChatGPT" id="editor"></div>
<button onclick="send()" id="send">Send</button>
<button onclick="editor.textContent = ''">Reset fixture draft</button>
<script>
let count = 0;
function send() {
  count++;
  const submission = count;
  const prompt = editor.innerText;
  editor.innerText = '';
  const user = document.createElement('section');
  user.innerHTML = '<h2>You said:</h2><p></p><button>Copy message</button>';
  user.querySelector('p').textContent = prompt;
  thread.append(user);
  const assistant = document.createElement('section');
  assistant.innerHTML = '<h2>ChatGPT said:</h2><p>Thinking</p><button>Stop generating</button>';
  thread.append(assistant);
  setTimeout(() => {
    assistant.querySelector('p').textContent = 'Fixture answer ' + submission + ' 中文: ' + 'word '.repeat(1600);
    assistant.querySelector('button').textContent = 'Copy';
  }, 900);
}
</script>
'@
[IO.File]::WriteAllText($page, $fixture)
$arguments = '--user-data-dir="{0}" --no-first-run --no-default-browser-check --force-renderer-accessibility "{1}"' -f (Join-Path $folder 'profile'), $page
$startInfo = [Diagnostics.ProcessStartInfo]::new($edge, $arguments)
$startInfo.UseShellExecute = $false
$launcher = [Diagnostics.Process]::Start($startInfo)
try {
    $script:fixtureDocument = $null
    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        Start-Sleep -Milliseconds 350
        $launcher.Refresh()
        if ($launcher.MainWindowHandle -eq 0) { continue }
        $root = [Windows.Automation.AutomationElement]::FromHandle($launcher.MainWindowHandle)
        $script:fixtureDocument = Edge-ChatDocument $root
    } until (($script:fixtureDocument -and (Edge-Editor $script:fixtureDocument)) -or $clock.Elapsed.TotalSeconds -ge 15)
    Assert-EdgeTest ([bool](Edge-Editor $script:fixtureDocument)) 'Fixture editor unavailable.'

    # 测试页只替换 URL 和窗口绑定，输入、发送和回答提取仍走真实 Edge 的 UIA。
    function Edge-Context([bool]$preparing) {
        @{doc=$script:fixtureDocument; address=[uri]'https://chatgpt.com/c/00000000-0000-4000-8000-000000000000'}
    }
    function Short([string]$value, [int]$max) {
        $value.Substring(0, [Math]::Min($value.Length, $max))
    }
    $picker = Edge-FirstNamed $script:fixtureDocument $edgePicker
    $fixtureModel = Edge-Model $picker
    Assert-EdgeTest ($fixtureModel -eq '6 Pro') ('Fixture model label: ' + $fixtureModel)
    $script:edgeChatTask = @{window=1; path='/'; model=$fixtureModel; userCount=0; stable=0; lastAnswer=$null}
    $editor = Edge-Editor $script:fixtureDocument
    $value = $editor.GetCurrentPattern($edgeValue)
    $value.SetValue('preserved draft')
    try {
        Edge-Send @{prompt='fixture question'} | Out-Null
        throw 'Edge-Send overwrote the draft.'
    } catch {
        Assert-EdgeTest ($_.Exception.Message -like 'edge_draft_present:*') $_.Exception.Message
    }
    Assert-EdgeTest ($value.Current.Value -eq 'preserved draft') 'Draft was mutated.'
    $reset = Edge-FirstNamed $script:fixtureDocument @('Reset fixture draft')
    $reset.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    Start-Sleep -Milliseconds 350
    $send = Edge-Send @{prompt='fixture question 中文'}
    Assert-EdgeTest ($send.model -eq '6 Pro') 'Incorrect verified model.'
    Assert-EdgeTest (!(Edge-Probe).finished) 'Generation returned an early result.'
    $clock.Restart()
    do {
        Start-Sleep -Milliseconds 500
        $progress = Edge-Probe
    } until ($progress.finished -or $clock.Elapsed.TotalSeconds -ge 12)
    Assert-EdgeTest $progress.finished 'The complete answer was not extracted.'
    $answer = $script:edgeChatTask.result.answer
    Assert-EdgeTest ($answer.StartsWith('Fixture answer 1 中文:') -and $answer.Length -gt 7500 -and !$answer.Contains('Copy')) 'The reply boundaries or Unicode contents are wrong.'
    Write-Output 'PASS real Edge UIA draft guard, verified submission, streaming wait and answer extraction'
} finally {
    $launcher.Refresh()
    if (!$launcher.HasExited) { [void]$launcher.CloseMainWindow() }
    $launcher.Dispose()
}
