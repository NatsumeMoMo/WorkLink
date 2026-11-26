Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
$edgeScope = [Windows.Automation.TreeScope]::Descendants
$edgeName = [Windows.Automation.AutomationElement]::NameProperty
$edgeType = [Windows.Automation.AutomationElement]::ControlTypeProperty
$edgeValue = [Windows.Automation.ValuePattern]::Pattern
$edgeText = [Windows.Automation.TextPattern]::Pattern
$edgeStart = [Windows.Automation.Text.TextPatternRangeEndpoint]::Start
$edgeEnd = [Windows.Automation.Text.TextPatternRangeEndpoint]::End
$edgePicker = @('选择 ChatGPT 模型', 'Choose ChatGPT model', 'Select ChatGPT model', 'Model selector', '选择模型')
$edgeComposer = @('询问 ChatGPT', 'Ask ChatGPT', 'Ask anything', 'Message ChatGPT', '问任何问题')
$edgeSend = @('发送', '发送提示', 'Send', 'Send prompt', 'Send message')
$edgeStop = @('停止', '停止生成', '停止响应', '停止流式传输', 'Stop generating', 'Stop response', 'Stop streaming')
$edgeUser = @('你说：', '你说:', 'You said:', 'You said')
$edgeAssistant = @('ChatGPT 说：', 'ChatGPT 说:', 'ChatGPT said:', 'ChatGPT said')
$edgeCopyUser = @('复制消息', 'Copy message', '编辑消息', 'Edit message')
$edgeCopyAnswer = @('复制', '复制回答', '复制回复', 'Copy', 'Copy response', 'Copy answer')
$edgeAccount = @('登录', 'Log in', 'Sign in', '免费注册', 'Sign up')
$edgeConversationPath = '^/c/[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}$'
$script:edgeChatTask = $null

function Edge-Named($element, [string[]]$labels) {
    $conditions = [Windows.Automation.Condition[]]@(
        foreach ($label in $labels) {
            [Windows.Automation.PropertyCondition]::new($edgeName, $label)
        }
    )
    if ($conditions.Length -eq 1) {
        return $element.FindAll($edgeScope, $conditions[0])
    }
    return $element.FindAll($edgeScope, [Windows.Automation.OrCondition]::new($conditions))
}

function Edge-FirstNamed($element, [string[]]$labels) {
    Edge-Named $element $labels | Select-Object -First 1
}

function Edge-Typed($element, $type) {
    $element.FindAll($edgeScope, [Windows.Automation.PropertyCondition]::new($edgeType, $type))
}

function Edge-GetUri([string]$address) {
    if ($address -notmatch '^[a-z][a-z0-9+.-]*://') { $address = 'https://' + $address }
    $uri = $null
    if (![Uri]::TryCreate($address, [UriKind]::Absolute, [ref]$uri) -or
        $uri.Scheme -ne 'https' -or $uri.Host -ne 'chatgpt.com' -or
        !$uri.IsDefaultPort -or $uri.UserInfo) { return }
    return $uri
}

function Edge-Address($root) {
    $bar = Edge-FirstNamed $root @('地址和搜索栏', 'Address and search bar', '網址列和搜尋列')
    if (!$bar) {
        $id = [Windows.Automation.AutomationElement]::AutomationIdProperty
        $bar = $root.FindFirst($edgeScope, [Windows.Automation.PropertyCondition]::new($id, 'view_1017'))
    }
    $pattern = $null
    if ($bar -and $bar.TryGetCurrentPattern($edgeValue, [ref]$pattern)) {
        Edge-GetUri $pattern.Current.Value
    }
}

function Edge-SelectedTab($root) {
    $tabs = Edge-Typed $root ([Windows.Automation.ControlType]::TabItem)
    foreach ($tab in $tabs) {
        $p = $null
        if ($tab.TryGetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern, [ref]$p) -and $p.Current.IsSelected) {
            return $tab.GetRuntimeId() -join '.'
        }
    }
}

function Edge-WindowList($processIds) {
    $desktop = [Windows.Automation.AutomationElement]::RootElement
    $roots = $desktop.FindAll([Windows.Automation.TreeScope]::Children, [Windows.Automation.Condition]::TrueCondition)
    foreach ($root in $roots) {
        try {
            $c = $root.Current
            if ($c.NativeWindowHandle -ne 0 -and $c.ClassName -eq 'Chrome_WidgetWin_1' -and
                $c.ProcessId -in $processIds -and $c.Name) {
                [pscustomobject]@{ handle = [long]$c.NativeWindowHandle; root = $root; pid = $c.ProcessId }
            }
        } catch [Windows.Automation.ElementNotAvailableException] {}
    }
}

function Edge-Profile($root) {
    $path = Join-Path $env:LOCALAPPDATA 'Microsoft\Edge\User Data\Local State'
    if (!(Test-Path -LiteralPath $path)) { throw 'edge_profile_unavailable: 无法读取 Edge 配置名称' }
    # 只用配置名称定位已有会话，浏览器自行维持登录。
    $state = [IO.File]::ReadAllText($path) | ConvertFrom-Json
    $profiles = @($state.profile.info_cache.PSObject.Properties | Where-Object { $_.Name -match '^(Default|Profile [0-9]+)$' })
    if ($profiles.Count -eq 1) { return $profiles[0].Name }
    $profileButton = $root.FindFirst($edgeScope, [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ClassNameProperty, 'EdgeAvatarToolbarButton'))
    if ($profileButton) {
        $title = $profileButton.Current.Name
        $profiles = @($profiles | Where-Object {
            $name = [string]$_.Value.name
            $name -and ($title -eq $name -or $title.StartsWith($name + ' ', [StringComparison]::OrdinalIgnoreCase))
        })
        if ($profiles.Count -eq 1) { return $profiles[0].Name }
    }
    throw 'edge_profile_ambiguous: 无法确认所选窗口的 Edge 配置，请保留一个普通配置窗口后重试'
}

function Edge-ChatDocument($root) {
    $root.FindFirst($edgeScope, [Windows.Automation.PropertyCondition]::new($edgeType, [Windows.Automation.ControlType]::Document))
}

function Edge-DocumentAddress($document) {
    $pattern = $null
    if ($document.TryGetCurrentPattern($edgeValue, [ref]$pattern)) {
        Edge-GetUri $pattern.Current.Value
    }
}

function Edge-RequestedDocument($root, [string]$path, [string]$fragment) {
    $documents = @(Edge-Typed $root ([Windows.Automation.ControlType]::Document))
    # Chromium 也会暴露后台标签的文档。按新标签的地址标记定位，不能取第一个 Document。
    foreach ($document in $documents) {
        try {
            $address = Edge-DocumentAddress $document
            if ($address -and $address.AbsolutePath -eq $path -and $address.Fragment -ceq $fragment) {
                return $document
            }
        } catch [Windows.Automation.ElementNotAvailableException] {}
    }
    # 某些版本的文档 URL 省略片段，只有活动内容容器中的同路径文档可以回退使用。
    $view = $root.FindFirst($edgeScope, [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ClassNameProperty, 'MultiContentsView'))
    if (!$view) { return }
    $found = @(foreach ($document in (Edge-Typed $view ([Windows.Automation.ControlType]::Document))) {
        $address = Edge-DocumentAddress $document
        if ($address -and $address.AbsolutePath -eq $path -and !$address.Fragment) { $document }
    })
    if ($found.Count -eq 1) { return $found[0] }
}

function Edge-ModelMatches([string]$actual, [string]$requested) {
    if ($actual.Trim() -eq $requested.Trim()) { return $true }
    $label = $actual -replace '^(?:GPT[- ]?)?[0-9]+(?:\.[0-9]+)*\s+', ''
    switch ($requested.Trim().ToLowerInvariant()) {
        'pro' { return $label -eq 'Pro' }
        'thinking' { return $label -in @('Thinking', '思考', '深度思考') }
        'instant' { return $label -in @('Instant', '快速', '即时') }
        'auto' { return $label -in @('Auto', '自动') }
        default { return $false }
    }
}

function Edge-Model($picker) {
    if (!$picker) { return }
    $labels = @(Edge-Typed $picker ([Windows.Automation.ControlType]::Text) | ForEach-Object { $_.Current.Name.Trim() } | Where-Object { $_ })
    if ($labels.Count -gt 0) { return Short ($labels -join ' ') 80 }
}

function Edge-SelectModel($doc, $picker, [string]$requested) {
    $model = Edge-Model $picker
    if (!$requested -or (Edge-ModelMatches $model $requested)) { return $model }
    $expand = $null
    if (!$picker -or !$picker.TryGetCurrentPattern([Windows.Automation.ExpandCollapsePattern]::Pattern, [ref]$expand)) {
        throw 'edge_model_unavailable: 无法打开模型选择器，请在 Edge 中选好模型'
    }
    $expand.Expand()
    try {
        $choices = @()
        $clock = [Diagnostics.Stopwatch]::StartNew()
        do {
            $choices = @(foreach ($type in @([Windows.Automation.ControlType]::MenuItem, [Windows.Automation.ControlType]::RadioButton, [Windows.Automation.ControlType]::ListItem)) {
                foreach ($item in (Edge-Typed $doc $type)) {
                    $firstLine = ($item.Current.Name -split '\r?\n', 2)[0]
                    if ($item.Current.IsEnabled -and (Edge-ModelMatches $firstLine $requested)) { $item }
                }
            })
            if ($choices.Count -gt 0) { break }
            Start-Sleep -Milliseconds 250
        } while ($clock.ElapsedMilliseconds -lt 3500)
        if ($choices.Count -ne 1) {
            throw 'edge_model_unavailable: 网页没有显示唯一可选的指定模型，请在 Edge 中核对权限或手动选择'
        }
        $command = $null
        if ($choices[0].TryGetCurrentPattern([Windows.Automation.InvokePattern]::Pattern, [ref]$command)) { $command.Invoke() }
        elseif ($choices[0].TryGetCurrentPattern([Windows.Automation.SelectionItemPattern]::Pattern, [ref]$command)) { $command.Select() }
        else { throw 'edge_model_unavailable: 网页未提供该模型的选取操作，请在 Edge 中手动选择' }
        $clock.Restart()
        do {
            Start-Sleep -Milliseconds 250
            $model = Edge-Model (Edge-FirstNamed $doc $edgePicker)
            if (Edge-ModelMatches $model $requested) { return $model }
        } while ($clock.ElapsedMilliseconds -lt 3500)
        throw 'edge_model_unverified: 网页未确认指定模型，问题未发送'
    } finally {
        try {
            if ($expand.Current.ExpandCollapseState -eq [Windows.Automation.ExpandCollapseState]::Expanded) { $expand.Collapse() }
        } catch {}
    }
}

function Edge-Editor($doc) {
    $editors = @(Edge-Named $doc $edgeComposer | Where-Object {
        $_.Current.ControlType -eq [Windows.Automation.ControlType]::Edit -and
        !$_.Current.IsPassword -and $_.Current.IsEnabled
    })
    if ($editors.Count -eq 1) { return $editors[0] }
}

function Edge-Buttons($doc, [string[]]$labels) {
    Edge-Named $doc $labels | Where-Object { $_.Current.ControlType -eq [Windows.Automation.ControlType]::Button }
}

function Edge-UserHeadings($doc) {
    Edge-Named $doc $edgeUser | Where-Object { $_.Current.ControlType -eq [Windows.Automation.ControlType]::Text }
}

function Edge-Prepare($request) {
    $path = '/'
    if ($request.conversation) {
        $uri = Edge-GetUri $request.conversation
        if (!$uri -or $uri.AbsolutePath -notmatch $edgeConversationPath -or $uri.Query -or $uri.Fragment) {
            throw 'invalid_conversation: 需要 ChatGPT 返回的 https://chatgpt.com/c/<UUID> 链接'
        }
        $path = $uri.AbsolutePath
    }
    $session = [Diagnostics.Process]::GetCurrentProcess().SessionId
    $processes = @(Get-CimInstance Win32_Process -Filter "Name='msedge.exe'" | Where-Object {
        $_.SessionId -eq $session -and $_.ExecutablePath -and $_.CommandLine -and
        $_.CommandLine -notmatch '(?i)(?:^|\s)--(?:type|user-data-dir|remote-debugging-port|headless|inprivate|guest)(?:\s|=|$)'
    })
    $windows = @(Edge-WindowList @($processes.ProcessId))
    if ($windows.Count -eq 0) { throw 'edge_window_not_found: 请在目标桌面打开普通 Edge 窗口' }
    if (!$request.window -and $windows.Count -gt 1) {
        $handles = ($windows | Select-Object -First 20 -ExpandProperty handle) -join ', '
        throw "edge_window_ambiguous: 请选择 --window。可用句柄：$handles"
    }
    $target = if ($request.window) { $windows | Where-Object handle -eq $request.window | Select-Object -First 1 } else { $windows[0] }
    if (!$target) { throw 'edge_window_not_found: 指定句柄不是此会话的普通 Edge 窗口' }
    $profile = Edge-Profile $target.root
    $previousTab = Edge-SelectedTab $target.root
    $process = $processes | Where-Object ProcessId -eq $target.pid | Select-Object -First 1
    $fragment = '#worklink-' + $request.task
    $url = 'https://chatgpt.com' + $path + $fragment
    # 给新标签加一次性标记，避免把问题填入用户已有的任何标签。
    $arguments = '--profile-directory="{0}" {1}' -f $profile, $url
    $info = [Diagnostics.ProcessStartInfo]::new($process.ExecutablePath, $arguments)
    $info.UseShellExecute = $false
    $info.CreateNoWindow = $true
    $launcher = [Diagnostics.Process]::Start($info)
    $launcher.Dispose()
    $tab = $null
    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        Start-Sleep -Milliseconds 400
        Check-Desktop
        $root = Window $target.handle
        $address = Edge-Address $root
        if (!$address -or $address.Fragment -cne $fragment -or $address.AbsolutePath -ne $path) { continue }
        $tab = Edge-SelectedTab $root
        if (!$tab -or $tab -ceq $previousTab) { continue }
        $doc = Edge-RequestedDocument $root $path $fragment
        if (!$doc) { continue }
        if (@(Edge-Named $doc $edgeAccount).Count -gt 0) {
            throw 'edge_login_required: 请在这台电脑的普通 Edge 中登录 ChatGPT'
        }
        $editor = Edge-Editor $doc
        $picker = Edge-FirstNamed $doc $edgePicker
        if ($editor -and $picker) { break }
    } while ($clock.Elapsed.TotalSeconds -lt 22)
    if (!$address -or $address.Fragment -cne $fragment -or !$tab -or $tab -ceq $previousTab -or !$doc) {
        throw 'edge_tab_unavailable: 新标签未在指定窗口就绪，请激活该 Edge 窗口后重试'
    }
    if (!$editor -or !$picker) { throw 'edge_chat_unavailable: 找不到登录后的 ChatGPT 输入框或模型选择器' }
    if (@(Edge-Buttons $doc $edgeStop).Count -gt 0) { throw 'edge_chat_busy: 当前对话已有回答在生成' }
    $value = $editor.GetCurrentPattern($edgeValue)
    if ($value.Current.IsReadOnly -or ![string]::IsNullOrWhiteSpace($value.Current.Value)) {
        throw 'edge_draft_present: ChatGPT 输入框已有草稿或不可编辑'
    }
    $model = Edge-SelectModel $doc $picker $request.model
    if (!$model) { throw 'edge_model_unverified: 网页未显示当前模型' }
    $script:edgeChatTask = @{
        window = $target.handle; tab = $tab; doc = $doc; path = $path; fragment = $fragment;
        model = $model; userCount = @(Edge-UserHeadings $doc).Count; stable = 0; lastAnswer = $null
    }
    return @{ window = $target.handle; model = $model }
}

function Edge-Context([bool]$preparing) {
    if (!$script:edgeChatTask) { throw 'edge_task_unavailable: Edge 任务尚未准备完成' }
    $task = $script:edgeChatTask
    $root = Window $task.window
    if ((Edge-SelectedTab $root) -cne $task.tab) { throw 'edge_tab_changed: Edge 当前标签已切换，请先确认原问题状态' }
    $doc = $task.doc
    # 后台窗口的地址栏可能保留旧 URL。用文档的实际 URL 识别 ChatGPT 的单页导航。
    $address = Edge-DocumentAddress $doc
    if (!$address) { throw 'edge_tab_changed: 原标签已离开 ChatGPT，或文档未提供地址' }
    if ($preparing) {
        $openingAddress = Edge-Address $root
        if (!$openingAddress -or $openingAddress.Fragment -cne $task.fragment -or $address.AbsolutePath -ne $task.path) {
            throw 'edge_tab_changed: 提交前所选标签已导航'
        }
    }
    if (!$preparing -and $task.path -ne '/' -and $address.AbsolutePath -ne $task.path) {
        throw 'edge_tab_changed: 已离开任务所绑定的对话'
    }
    return @{ doc = $doc; address = $address }
}

function Edge-Normalize([string]$text) {
    (($text -replace '\uFFFC', '') -replace '\s+', ' ').Trim()
}

function Edge-Send($request) {
    $task = $script:edgeChatTask
    $active = Edge-Context $true
    $editor = Edge-Editor $active.doc
    if (!$editor) { throw 'edge_composer_unavailable: 找不到唯一的 ChatGPT 输入框' }
    $value = $editor.GetCurrentPattern($edgeValue)
    if ($value.Current.IsReadOnly -or ![string]::IsNullOrWhiteSpace($value.Current.Value)) {
        throw 'edge_draft_present: ChatGPT 输入框已有草稿或不可编辑'
    }
    $scroll = $null
    if ($editor.TryGetCurrentPattern([Windows.Automation.ScrollItemPattern]::Pattern, [ref]$scroll)) { $scroll.ScrollIntoView() }
    $value.SetValue([string]$request.prompt)
    $expected = Edge-Normalize $request.prompt
    $clock = [Diagnostics.Stopwatch]::StartNew()
    do {
        Start-Sleep -Milliseconds 250
        $active = Edge-Context $true
        $buttons = @(Edge-Buttons $active.doc $edgeSend | Where-Object { $_.Current.IsEnabled })
        if ($buttons.Count -eq 1) { break }
    } while ($clock.Elapsed.TotalSeconds -lt 5)
    if ($buttons.Count -ne 1) { throw 'edge_send_unavailable: 未找到唯一可用的发送按钮，请检查原标签中的草稿' }
    if ((Edge-Normalize $value.Current.Value) -cne $expected) { throw 'edge_draft_changed: 输入框内容已变化，未发送问题' }
    if ((Edge-Model (Edge-FirstNamed $active.doc $edgePicker)) -cne $task.model) {
        throw 'edge_model_changed: 发送前模型已变化，未发送问题'
    }
    if (@(Edge-Buttons $active.doc $edgeStop).Count -gt 0 -or @(Edge-UserHeadings $active.doc).Count -ne $task.userCount) {
        throw 'edge_chat_changed: 页面出现了其他对话操作，未发送问题'
    }
    $task.prompt = $expected
    $buttons[0].GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    return @{ model = $task.model }
}

function Edge-RangeAfter($pattern, $anchor, $element) {
    $range = $pattern.RangeFromChild($element)
    if ($range.CompareEndpoints($edgeStart, $anchor, $edgeEnd) -ge 0) { return $range }
}

function Edge-ReadBetween($pattern, $heading, $end, [int]$limit) {
    $range = $heading.Clone()
    $range.MoveEndpointByRange($edgeStart, $heading, $edgeEnd)
    $range.MoveEndpointByRange($edgeEnd, $end, $edgeStart)
    return $range.GetText($limit)
}

function Edge-Probe {
    $task = $script:edgeChatTask
    $active = Edge-Context $false
    $address = $active.address
    if ($address.AbsolutePath -notmatch $edgeConversationPath) { return @{ finished = $false } }
    if ($task.boundPath -and $task.boundPath -ne $address.AbsolutePath) {
        throw 'edge_conversation_changed: ChatGPT 已导航到其他对话'
    }
    $task.boundPath = $address.AbsolutePath
    $doc = $active.doc
    $users = @(Edge-UserHeadings $doc)
    if ($users.Count -le $task.userCount -or @(Edge-Buttons $doc $edgeStop).Count -gt 0) {
        $task.stable = 0
        return @{ finished = $false }
    }
    $pattern = $doc.GetCurrentPattern($edgeText)
    $user = $pattern.RangeFromChild($users[-1])
    $assistant = $null
    foreach ($heading in (Edge-Named $doc $edgeAssistant)) {
        if ($heading.Current.ControlType -ne [Windows.Automation.ControlType]::Text) { continue }
        $assistant = Edge-RangeAfter $pattern $user $heading
        if ($assistant) { break }
    }
    if (!$assistant) { return @{ finished = $false } }
    $userEnd = $assistant
    foreach ($button in (Edge-Buttons $doc $edgeCopyUser)) {
        $range = Edge-RangeAfter $pattern $user $button
        if ($range -and $range.CompareEndpoints($edgeStart, $assistant, $edgeStart) -lt 0) {
            $userEnd = $range
            break
        }
    }
    $seen = Edge-Normalize (Edge-ReadBetween $pattern $user $userEnd 65536)
    if ($seen -cne $task.prompt) { throw 'edge_conversation_changed: 最新一轮用户消息与本任务的问题不一致' }
    $copy = $null
    foreach ($button in (Edge-Buttons $doc $edgeCopyAnswer)) {
        $copy = Edge-RangeAfter $pattern $assistant $button
        if ($copy) { break }
    }
    if (!$copy) { return @{ finished = $false } }
    # 用回答区的复制按钮作边界，只读可访问文本，避免改动用户剪贴板或带回网页菜单。
    $limit = 524288
    $text = Edge-ReadBetween $pattern $assistant $copy ($limit + 1)
    $truncated = $text.Length -gt $limit
    if ($truncated) {
        $length = $limit
        if ([char]::IsHighSurrogate($text[$length - 1])) { $length-- }
        $text = $text.Substring(0, $length)
    }
    $text = ($text -replace '\uFFFC', '').Trim()
    if (!$text) { return @{ finished = $false } }
    if ($text -ceq $task.lastAnswer) { $task.stable++ } else { $task.stable = 0 }
    $task.lastAnswer = $text
    if ($task.stable -lt 2) { return @{ finished = $false } }
    $task.result = @{
        answer = $text; truncated = $truncated; window = $task.window;
        conversation = 'https://chatgpt.com' + $address.AbsolutePath
    }
    return @{ finished = $true }
}
