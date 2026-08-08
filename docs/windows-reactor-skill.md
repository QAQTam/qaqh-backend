# windows-reactor 开发 Skill

> 基于 microsoft/windows-rs master 分支源码研究整理（2026-08-06 抓取，crate 版本 0.100.0）
> 来源：`crates/libs/reactor/`（实现）、`docs/crates/windows-reactor.md`（官方指南）、`crates/samples/reactor/`（示例）

## 一句话心智模型

写一个 `fn(&mut RenderCx) -> Element` 的渲染函数，reactor 拿它的返回值和当前 WinUI 可视化树做 diff，只把变化的部分 patch 上去。状态放在 hooks 里，改状态就触发重渲染。**不需要 .xaml 文件**，全部是 Rust 构建器代码。

## 最小骨架

```toml
# Cargo.toml
[dependencies]
windows-reactor = "0.100"
[build-dependencies]
windows-reactor-setup = "0.100"

[profile.release]
panic = "abort"   # 见下方"错误模型"，release 建议加
```

```rust
use windows_reactor::*;

fn app(cx: &mut RenderCx) -> Element {
    let (count, set_count) = cx.use_state(0_i32);
    vstack((
        text_block(format!("count = {count}")).font_size(18.0).bold(),
        button("Click").on_click(move || set_count.call(count + 1)),
    ))
    .spacing(12.0)
    .into()
}

fn main() -> windows_core::Result<()> {
    bootstrap()?; // framework-dependent 部署模型才需要；self-contained 不调用
    App::new().title("My App").render(app)
}
```

`build.rs` 要用 `windows-reactor-setup` 的 `as_self_contained` / `as_framework_dependent` / `as_example` 三选一，负责把 Windows App SDK 运行时铺好。

## Hooks（状态管理）

| Hook | 作用 |
|---|---|
| `use_state(initial)` | `(value, SetState)`，`set.call(v)` 更新并触发重渲染 |
| `use_ref(initial)` | `HookRef`，改了**不**触发重渲染，适合帧计数器、缓存资源 |
| `use_memo(deps, factory)` | deps 不变就不重算 |
| `use_effect(deps, f)` / `use_effect_with_cleanup` | deps 变化时跑副作用 |
| `use_reducer` / `use_reducer_fn` | reducer 风格状态 |
| `use_resource(fetcher, deps)` | 返回 `Resource<T>`（`Loading`/`Ready`/`Error`），异步取数据的标准写法 |
| `use_async_state(initial)` | **关键**：返回的 `SetState` 是 `Send + Clone`，可以从**别的线程**调用 `.call()` 来驱动重渲染 |
| `use_context(&context)` | 读上层 provide 的值 |
| `use_open_window()` | 打开新的顶层窗口 |

## 流式输出怎么做（对接你们的 SSE/单节点更新协议）

关键就是 `use_async_state`：

```rust
let (text, set_text) = cx.use_async_state(String::new());

let start_stream = {
    let set_text = set_text.clone();
    move || {
        let set_text = set_text.clone();
        std::thread::spawn(move || {
            // 在这个后台线程里做 HttpClient 流式读取 / SSE 逐行解析
            let mut acc = String::new();
            for chunk in read_sse_stream() {
                acc.push_str(&chunk);
                set_text.call(acc.clone()); // 每收到一块就推一次
            }
        });
    }
};
```

这和你们 SolidJS2 那套"更新单个 signal → 精确更新一个 DOM 节点"是同构的：`set_text.call(..)` 只会让 reconciler 发现"这个 `text_block` 的 `Text` 属性变了"，只 patch 这一个属性的 COM 调用，不会重建整棵树。

`use_resource` 更适合"一次性异步取数据"（页面切换、初次加载），`use_async_state` 更适合"持续从后台线程推送增量"——你们的场景用后者。

**节流提醒**：token 级别高频更新如果每个 token 都 `set.call()`，会产生大量重渲染。参考"性能笔记"一节，reconciler 本身已经做了"未变化控件跳过 diff"，但如果你在一个渲染周期里多次 `set_state`，reactor 会把它们**在 dispatcher 层合并成一次渲染**（"State writes are coalesced through the dispatcher"），所以不用你自己写节流逻辑，高频 `set_text.call()` 是被官方设计允许的用法。

## UI 构建 & 样式

- 布局：`vstack((..))` / `hstack((..))` `.spacing(..)`；`grid((..))` 配 `.rows([..])` `.columns([..])`（`GridLength::STAR`/`Auto`）
- 约 60 个 WinUI 控件被包装：`check_box` `combo_box` `slider` `tree_view` `navigation_view` `tab_view` `text_box` `number_box` `color_picker` `content_dialog` `info_bar` `command_bar` 等，完整清单看 `crates/libs/reactor/src/widgets/`
- 通用修饰符（`ElementExt` trait，任何 `Element` 都能用）：`.margin()` `.padding()` `.width()` `.height()` `.background()` `.foreground()` `.opacity()` `.horizontal_alignment()` 等
- 轻量样式覆盖用 `resource_overrides`，只会覆盖 Reactor 自己写入过的 key，不影响其他人写的资源字典条目：
```rust
button("Delete").resource_overrides(|r| {
    r.set("ButtonBackground", Color::rgb(178, 34, 34))
     .set("ControlCornerRadius", CornerRadius::uniform(8.0))
});
```
- 进出场动画：`.transition(enter, exit)`，逻辑元素立刻移除，但 WinUI Composition 会让退场动画播完再真正销毁

## 事件处理的一个隐藏坑：handler identity

`use_state`/`use_reducer` 返回的 `SetState` 是**按 hook slot 记忆的**，直接传（`button(..).on_click(set_count)`）能让 reconciler 每次渲染拿到相同的 handler 身份，从而跳过整个控件的 diff。

如果写成 `move |v| set.call(v)` 这种内联闭包，**每次渲染都是新身份**，会导致这个控件每次都被重新 diff、WinUI 事件重新绑定一次。需要额外逻辑时用 `cx.use_callback(deps, ...)` 记忆化。

固定值场景有语法糖：`button("Reset").on_click(set_count.setter(0))`。

## 线程模型

- Reactor 跑在 WinUI 的 STA（单线程单元）线程上，per-thread 状态放 `thread_local!`
- 所有 window 共享同一个 UI 线程和消息循环（多窗口也不例外）
- 需要跨线程推送更新时用 `use_async_state`（见上），不要自己乱切线程碰 COM 对象——除了 `SetState.call()` 之外的 UI 操作必须回到 UI 线程

## 错误模型

- `bootstrap()` / `run()` 之外几乎不用 `Result`：渲染函数、事件回调里的 panic 被 reactor 在自己拥有的边界处（`Callback::invoke`、`DispatcherTimer`、`on_rendering`、渲染流程）捕获，转发给 `App::on_fault(|fault| ...)`（默认行为是记日志+继续，不会让整个进程崩)
- release 建议加 `panic = "abort"`：因为 fault 边界依赖 `panic = "unwind"`（Cargo 默认），一旦有 panic 逃出所有边界会在 WinUI 的 C++ 帧里 unwind，是未定义行为；显式 `abort` 让它老实崩溃退出而不是产生 UB
- `panic!` 只应该用于程序员错误（hooks 使用规则违反、类型不匹配），不是给业务逻辑用的

## 性能笔记（reconciler 内部）

- 稳态下（没有结构性变化）reconciler 只做"kind match + 浅比较"就跳过未变控件，**不创建新的 WinUI 控件**，成本主要在 COM 属性 set 调用上
- 没有 element pooling（稳态零创建，没什么好回收的）
- 没有重渲染递归深度保护：`set_state` 在渲染中被调用只是打个 dirty flag，通过 dispatcher 排队下一次渲染，不会递归重入，所以不可能出现无限递归
- 每个控件持有一个 `Handle` 枚举（具体 WinUI 类型），共享修饰符靠匹配 `Handle` 变体而不是 `cast` 探测接口——因为默认接口零 QI 开销，但 XAML 聚合对象上失败的 `QueryInterface` 很贵

## 样例仓库怎么用

`crates/samples/reactor/`：
- `samples/examples/`：约 90 个按控件/按 hook 拆分的最小示例（`counter` `calculator` `navigation_view` `use_resource` `async_state` `keyed_list_reorder` `lightweight_resources` `pointer_resize` `color_picker` `secondary_window` 等）—— **建议先喂这个目录给 AI**，信息密度高、每个文件只讲一件事，比啃 `crates/libs/reactor/src` 源码效率高得多
- `apps/`：完整应用（`notepad` `solitaire` `minesweeper` `tictactoe` `dotsweeper`）
- `gallery/`：WinUI Gallery 风格的控件浏览壳应用，规模最大，适合当作"这套框架撑不撑得住复杂应用"的参考
- `webview/`：`windows-webview` 的 `webview(on_ready)`，如果还没完全砍掉 WebView 渲染，混合过渡期可以用这个桥接

## 现实情况提醒（不是坑，是现状）

- 这是官方仓库（`microsoft/windows-rs`）下的项目，作者是微软内部人员，但目前版本号 0.100.0，API 仍在快速变动中（本次抓取时最后一次提交是**当天**）
- 没有 XAML 热重载/VS 设计器这类工具链，调 UI 全靠改代码重编译跑
- `list_view`/虚拟化长列表控件目前**没有**在已包装的 ~60 个控件清单里出现，如果聊天记录这类需要虚拟滚动的长列表场景要用到它，需要额外确认支持情况或自己包装
- Padding/Background/Foreground 等属性在不同控件上没有统一接口，reactor 内部按 `Handle` 变体分发，遇到"某个容器不支持某个通用修饰符"时会在 debug 模式下打警告（`diag::unhandled_modifier`）而不是报错，生产环境要留意这类静默降级

## 开发约定：rg 参数陷阱（2026-08-08 实测确认）

**`rg -rn` 不是"递归+行号"，而是 `--replace n`**——`-r` 在 ripgrep 里是 `--replace`（替换匹配文本），短选项组合 `-rn` 中 `r` 吞掉 `n` 作为替换文本，输出中所有匹配内容被替换为 `n`，且行号丢失。与 shell 无关（pwsh/bash/argv 直连三种模式实测结果一致），与 exec 解析无关。

| 选项 | GNU grep | ripgrep |
|---|---|---|
| `-r` | `--recursive` | `--replace`（rg 对目录默认递归，`-r` 被让给 replace） |
| `-n` | `--line-number` | `--line-number`（一致） |

**规范用法**（代码探索一律遵守）：

```bash
rg -n 'pattern' path          # ✅ 目录自动递归 + 行号，唯一需要的写法
rg --recursive --line-number 'pattern' path   # ✅ 想显式时用长参数
rg -rn 'pattern' path         # ❌ 禁止（= --replace n，静默污染输出）
```

真需要替换输出时用长参数 `--replace 'text'`，避免缩写歧义。排查定位请优先 `read_file`（输出不经 rg 替换，中文编码也稳定）。
