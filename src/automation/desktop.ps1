$ErrorActionPreference='Stop'
[Console]::InputEncoding=[Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName UIAutomationClient,UIAutomationTypes,System.Drawing
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class WorkLinkDesktop {
 [DllImport("user32.dll")] public static extern IntPtr OpenInputDesktop(uint flags,bool inherit,uint access);
 [DllImport("user32.dll")] public static extern bool CloseDesktop(IntPtr desktop);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern bool GetUserObjectInformation(IntPtr handle,int index,System.Text.StringBuilder text,uint length,out uint needed);
 [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr handle);
 [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr handle);
 [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr handle,int command);
}
'@
$refs=@{}
function Short([string]$value,[int]$max=100){$value=($value -replace '\s+',' ').Trim();if($value.Length -gt $max){$value=$value.Substring(0,$max)};return $value}
function Check-Desktop {
 $handle=[WorkLinkDesktop]::OpenInputDesktop(0,$false,1)
 if($handle -eq [IntPtr]::Zero){throw 'desktop_locked: 当前输入桌面不可访问'}
 try{$name=[Text.StringBuilder]::new(256);$needed=[uint32]0;if(![WorkLinkDesktop]::GetUserObjectInformation($handle,2,$name,512,[ref]$needed) -or $name.ToString() -ne 'Default'){throw 'desktop_locked: 需要已解锁的用户桌面'}}finally{[void][WorkLinkDesktop]::CloseDesktop($handle)}
}
function Window([long]$value){
 $handle=[IntPtr]::new($value)
 if(![WorkLinkDesktop]::IsWindow($handle)){throw 'window_not_found'}
 $element=[Windows.Automation.AutomationElement]::FromHandle($handle)
 $process=[Diagnostics.Process]::GetProcessById($element.Current.ProcessId)
 if($process.SessionId -ne [Diagnostics.Process]::GetCurrentProcess().SessionId){throw 'wrong_session'}
 return $element
}
function Reference([string]$key,[string]$peer){
 if(!$refs.ContainsKey($key) -or $refs[$key].peer -ne $peer -or ((Get-Date)-$refs[$key].created).TotalSeconds -gt 300){throw 'ref_expired: 请重新获取控件快照'}
 return $refs[$key].element
}
function Invoke-Desktop($r){
 Check-Desktop
 switch($r.kind){
  'desktop_windows' {
   $items=@();$used=100;$all=[Windows.Automation.AutomationElement]::RootElement.FindAll([Windows.Automation.TreeScope]::Children,[Windows.Automation.Condition]::TrueCondition)
   foreach($element in $all){
    try{
     $c=$element.Current
     if($c.NativeWindowHandle -eq 0 -or $c.IsOffscreen -or !$c.Name){continue}
     if([Diagnostics.Process]::GetProcessById($c.ProcessId).SessionId -ne [Diagnostics.Process]::GetCurrentProcess().SessionId){continue}
     $item=@{window=[long]$c.NativeWindowHandle;title=(Short $c.Name);pid=$c.ProcessId};$size=($item|ConvertTo-Json -Compress).Length
     if($items.Count -ge $r.budget.max_items -or $used+$size -gt $r.budget.max_chars){break};$items+=,$item;$used+=$size
    }catch{}
   }
   return @{windows=$items}
  }
  'desktop_launch' {
   $i=[Diagnostics.ProcessStartInfo]::new();$i.FileName=$r.program;$i.Arguments=$r.arguments;$i.UseShellExecute=$false;$i.CreateNoWindow=$true
   $p=[Diagnostics.Process]::Start($i);return @{launched=$true;pid=$p.Id}
  }
  'desktop_snapshot' {
   $root=Window $r.window
   foreach($key in @($refs.Keys)){if($refs[$key].peer -eq $r.peer -or ((Get-Date)-$refs[$key].created).TotalSeconds -gt 300){$refs.Remove($key)}}
   $queue=[Collections.Generic.Queue[object]]::new();$queue.Enqueue(@{element=$root;depth=0})
   $walker=[Windows.Automation.TreeWalker]::ControlViewWalker;$controls=@();$visited=0;$count=0;$used=250;$next=$null;$nonce=[Guid]::NewGuid().ToString('N').Substring(0,8)
   while($queue.Count -gt 0 -and $visited -lt 512){
    $entry=$queue.Dequeue();$visited++;$e=$entry.element
    try{
     $c=$e.Current
     if(!$c.IsOffscreen -and !$c.IsPassword){
      $patterns=@();$pattern=$null
      foreach($pair in @(@('invoke',[Windows.Automation.InvokePattern]::Pattern),@('value',[Windows.Automation.ValuePattern]::Pattern),@('toggle',[Windows.Automation.TogglePattern]::Pattern),@('select',[Windows.Automation.SelectionItemPattern]::Pattern))){if($e.TryGetCurrentPattern($pair[1],[ref]$pattern)){$patterns+=$pair[0]}}
      if($c.Name -or $patterns.Count -gt 0){
       $key=$nonce+':'+$count;$refs[$key]=@{peer=$r.peer;element=$e;created=(Get-Date)}
       $item=@{ref=$key;role=$c.ControlType.ProgrammaticName.Replace('ControlType.','');name=(Short $c.Name);patterns=$patterns;enabled=$c.IsEnabled}
       $size=($item|ConvertTo-Json -Compress -Depth 4).Length
       if($count -ge $r.offset){if($null -eq $next -and $controls.Count -lt $r.budget.max_items -and $used+$size -le $r.budget.max_chars){$controls+=,$item;$used+=$size}else{if($null -eq $next){$next=$count}}}
       $count++
      }
     }
     if($entry.depth -lt 6){$child=$walker.GetFirstChild($e);$siblings=0;while($null -ne $child -and $siblings -lt 128){$queue.Enqueue(@{element=$child;depth=$entry.depth+1});$child=$walker.GetNextSibling($child);$siblings++}}
    }catch{}
   }
   return @{window=$r.window;title=(Short $root.Current.Name);revision=$nonce;controls=$controls;next_offset=$next;total_controls=$count;source_truncated=($queue.Count -gt 0)}
  }
  'desktop_invoke' {
   $e=Reference $r.reference $r.peer;$p=$null
   if($e.TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern,[ref]$p)){$p.Invoke()}
   elseif($e.TryGetCurrentPattern([Windows.Automation.TogglePattern]::Pattern,[ref]$p)){$p.Toggle()}
   elseif($e.TryGetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern,[ref]$p)){$p.Select()}
   else{throw 'pattern_not_supported: 控件未提供可调用操作'}
   return @{invoked=$true}
  }
  'desktop_fill' {
   $e=Reference $r.reference $r.peer;$p=$null
   if($e.Current.IsPassword){throw 'password_control: 请在本机完成密码输入'}
   if(!$e.TryGetCurrentPattern([Windows.Automation.ValuePattern]::Pattern,[ref]$p)){throw 'pattern_not_supported: 控件未提供 ValuePattern'}
   $p.SetValue([string]$r.text);return @{filled=$true}
  }
  'desktop_focus' {
   $e=Window $r.window;$handle=[IntPtr]::new([long]$r.window);[void][WorkLinkDesktop]::ShowWindow($handle,9)
   if(![WorkLinkDesktop]::SetForegroundWindow($handle)){throw 'focus_denied: Windows 拒绝切换前台窗口'}
   return @{focused=$true}
  }
  'desktop_screenshot' {
   $e=Window $r.window;$rect=$e.Current.BoundingRectangle
   if($e.Current.IsOffscreen -or $rect.Width -le 0 -or $rect.Height -le 0){throw 'window_not_visible'}
   $width=[int][Math]::Min($rect.Width,8192);$height=[int][Math]::Min($rect.Height,8192)
   if([long]$width*$height -gt 24000000){throw 'window_too_large'}
   $bitmap=[Drawing.Bitmap]::new($width,$height)
   try{
    $g=[Drawing.Graphics]::FromImage($bitmap)
    try{$g.CopyFromScreen([int]$rect.X,[int]$rect.Y,0,0,$bitmap.Size)}finally{$g.Dispose()}
    $scale=[Math]::Min(1,[Math]::Min(1280/$width,900/$height));$small=[Drawing.Bitmap]::new($bitmap,[Drawing.Size]::new([int]($width*$scale),[int]($height*$scale)))
    try{
     $codec=[Drawing.Imaging.ImageCodecInfo]::GetImageEncoders()|Where-Object MimeType -eq 'image/jpeg'
     $parameters=[Drawing.Imaging.EncoderParameters]::new(1);$parameters.Param[0]=[Drawing.Imaging.EncoderParameter]::new([Drawing.Imaging.Encoder]::Quality,[long]65)
     try{$small.Save([string]$r.output,$codec,$parameters)}finally{$parameters.Dispose()}
     return @{width=$small.Width;height=$small.Height}
    }finally{$small.Dispose()}
   }finally{$bitmap.Dispose()}
  }
  default{throw 'unsupported_desktop_action'}
 }
}
while($null -ne ($line=[Console]::ReadLine())){
 try{$request=$line|ConvertFrom-Json;$data=Invoke-Desktop $request;$response=@{ok=$true;data=$data}}
 catch{$response=@{ok=$false;error=(Short $_.Exception.Message 500)}}
 [Console]::WriteLine(($response|ConvertTo-Json -Depth 12 -Compress))
}
