# dxpdf-mt 项目介绍

dxpdf-mt 是一个用 Rust 编写、由 Skia 驱动的独立 DOCX 转 PDF 引擎。它直接解析
Office Open XML，不依赖 Microsoft Office、LibreOffice、WPS 或云端转换服务，可通过
命令行、Rust API 和 Python 包使用。

本仓库基于 MIT 许可的上游 dxpdf 0.4，重点补强真实 Word/WPS 文档兼容性、受控字体、
复杂页面布局、发布工程和批量转换稳定性。目前它是正在验证的兼容性候选版本，不承诺
任意 DOCX 都能与 Word/WPS 像素级完全一致。

## 项目目标

- 在本地或服务器内完成 DOCX 转 PDF，不上传业务文档。
- 为中英文业务文档提供可复现、可审计的转换结果。
- 允许应用显式指定字体，不修改操作系统字体。
- 对 ZIP、媒体、SVG 等输入实施资源上限和安全检查。
- 提供可直接安装的 Python wheel，并通过 GitHub Actions 自动构建多平台产物。
- 通过真实文档、合成夹具和视觉对照持续提高 Word/WPS 兼容性。

## 核心能力

| 类别 | 当前能力 |
|---|---|
| 文字 | 字体、字号、粗体、斜体、下划线、颜色、字符间距、上下标、语言与主题字体 |
| 段落 | 对齐、缩进、制表位、行距、段前段后、边框、底纹、分页和孤行控制 |
| 表格 | 表格样式、单元格边距、合并、跨页拆分、重复表头、嵌套表格和浮动表格 |
| 图像 | PNG、JPEG、GIF、BMP、WebP、部分 EMF、DrawingML 图片、裁剪和浮动环绕 |
| 矢量与形状 | SVG 优选源、DrawingML/VML 形状、文本框、部分组合图形和 WordArt |
| 页面 | 页眉页脚、页码字段、分节、奇偶页、横竖版、多栏和页面背景 |
| 字体 | DOCX 内嵌字体、进程内自定义字体目录、系统字体、字体替代和 PDF 子集嵌入 |
| 国际化 | 中英文、东亚字体槽、Unicode 字素、彩色 emoji 和多级编号 |
| 输出 | Skia PDF、可提取文本、字体子集、链接、书签和内部跳转 |

## 转换架构

```text
DOCX bytes
  -> 有界 ZIP 解包与 OOXML 解析
  -> 与解析器解耦的 Document 模型
  -> 样式、主题、关系、颜色、字体和分节解析
  -> Skia 字体测量与页面布局
  -> 每份文档独立的字体子集
  -> DrawCommand 绘制
  -> PDF bytes
```

主要代码边界：

| 层 | 目录 | 作用 |
|---|---|---|
| 公共接口 | `src/lib.rs`、`src/main.rs` | Rust、CLI、Python 入口 |
| DOCX 解析 | `src/docx/` | ZIP、关系、MCE 和 OOXML schema |
| 文档模型 | `src/model/` | 与解析器解耦的数据类型及尺寸单位 |
| 属性解析 | `src/render/resolve/` | 样式级联、颜色、字体、图片和分节 |
| 页面布局 | `src/render/layout/` | 段落、表格、浮动对象、脚注和分页 |
| 字体子集 | `src/render/subset/` | 收集本次输出实际使用的字形 |
| PDF 绘制 | `src/render/painter.rs` | Skia 绘制和 PDF 输出边界 |
| Python 包 | `python/dxpdf/` | Python API、字体路径和 wheel 内资源 |

## Python 使用

安装发布后的 wheel：

```bash
pip install dxpdf
```

文件转换：

```python
import dxpdf

dxpdf.convert_file("input.docx", "output.pdf")
```

内存转换：

```python
from pathlib import Path
import dxpdf

docx_bytes = Path("input.docx").read_bytes()
pdf_bytes = dxpdf.convert(docx_bytes, image_dpi=300)
Path("output.pdf").write_bytes(pdf_bytes)
```

### 自定义字体

调用时可以传入一个字体目录：

```python
dxpdf.convert_file(
    "input.docx",
    "output.pdf",
    font_dir=r"D:\fonts",
)
```

也可以按优先级传入多个目录，越靠前优先级越高：

```python
dxpdf.convert_file(
    "input.docx",
    "output.pdf",
    font_dir=[r"D:\customer-fonts", r"D:\open-fonts"],
)
```

如果希望字体长期自动生效，可以把自己有权使用的 `.ttf`、`.otf` 或 `.ttc` 文件复制到：

```python
import dxpdf

print(dxpdf.user_fonts_path())
```

也可以设置 `DXPDF_FONT_DIR`。多个目录使用当前平台的路径分隔符：Windows 使用`;`，
Linux/macOS 使用 `:`。

字体查找顺序如下：

1. DOCX 内嵌字体；
2. 本次调用的 `font_dir`；
3. `DXPDF_FONT_DIR`；
4. dxpdf 用户字体目录；
5. wheel 内置字体目录；
6. 系统字体；
7. dxpdf 字体替代和默认回退。

所有外部字体只在当前进程内加载。dxpdf 不会安装、删除或修改操作系统字体。wheel 已包含
字体目录、manifest 和许可证目录，但通用开源字体只有在许可与来源审计完成后才会加入。

## CLI 使用

```bash
dxpdf input.docx
dxpdf input.docx -o output.pdf
dxpdf input.docx --font-dir ./fonts
dxpdf input.docx --image-dpi 300
```

`image-dpi` 默认是 220。提高它可改善位图打印清晰度，但会增加 PDF 体积；源图片不足时
不会被无意义地放大。

## Rust 使用

```rust
let docx_bytes = std::fs::read("input.docx")?;
let pdf_bytes = dxpdf::convert(&docx_bytes)?;
std::fs::write("output.pdf", pdf_bytes)?;
```

多个受控字体目录可以按顺序加载：

```rust
use dxpdf::{PackageLimits, RenderOptions};

let pdf_bytes = dxpdf::convert_with_options_and_font_dirs(
    &docx_bytes,
    &RenderOptions::default(),
    &PackageLimits::default(),
    ["customer-fonts", "open-fonts"],
)?;
```

## 本地构建

Rust CLI：

```bash
cargo build --release
cargo test --all
```

Python wheel：

```bash
python -m pip install maturin
maturin build --release --features python
```

Python 扩展使用 CPython stable ABI，构建产物为 `cp38-abi3` wheel，可覆盖 Python 3.8
及更新版本，而不必为每个 CPython 小版本分别编译。

## GitHub 与 PyPI 发布

仓库包含两条 GitHub Actions 流程：

- `ci.yml`：格式、测试、文档、普通构建和多平台 wheel 安装验证；
- `python.yml`：GitHub Release 发布时构建 wheel、sdist，并通过受信任发布或令牌上传 PyPI。

当前 wheel 矩阵覆盖：

- Windows x86_64；
- Linux x86_64；
- Linux ARM64；
- macOS Intel；
- macOS Apple Silicon。

正式发布前必须确认 PyPI distribution 名称、项目所有权、签名/权限和字体许可证。Python
导入名可以继续保持 `dxpdf`，即使最终 distribution 名需要与上游项目区分。

## 质量保证

项目不会只以“能打开 PDF”作为通过标准。验证通常包含：

- Rust 单元、集成和文档测试；
- Python API 与 wheel 内容测试；
- 全新虚拟环境安装；
- 相同输入重复转换的确定性检查；
- 页面数量、尺寸、文本提取和对象几何检查；
- 关键页面的 144/288 DPI 栅格视觉对照；
- 英文、中文、业务模板和复杂 ONLYOFFICE 文档回归；
- 字体包、二进制和输出文件 SHA-256 记录。

缺字、内容丢失、裁切、重叠和业务语义变化属于高优先级问题；细微间距和像素差异不会与
这些问题混成一个分数。

## 已知边界

- 任意 DOCX 与特定 Word/WPS 版本的像素级一致仍是长期目标。
- 缺失字体或字体版本不同会改变换行、分页和字形。
- 复杂 AutoFit 表格、深层组合图形、部分 VML、垂直书写和多对象环绕仍有长尾差异。
- SVG 与外部媒体按安全白名单处理，不支持的内容会回退到文档提供的普通图片源。
- 公共服务器应在隔离工作进程中转换，并由上层负责超时、内存、并发和进程回收。

## 仓库结构

```text
src/                 Rust 转换引擎
python/dxpdf/        Python 公共包与原生扩展入口
python/tests/        Python API 测试
docs/                架构、兼容性、发布门禁和专题设计文档
.github/workflows/   CI 与多平台发布自动化
scripts/             验证、构建和回归辅助脚本
```

客户文档、私有语料、字体包、生成的 PDF、构建缓存和工具链不应提交到仓库。

## 许可证与上游

本仓库与上游 dxpdf 均采用 MIT 许可证。新增字体不自动继承代码许可证：每个随 wheel 分发
的字体都必须单独记录名称、版本、来源、哈希、许可证和再分发条件。

- 本仓库：<https://github.com/mtt-mtt/dxpdf-mt>
- 上游项目：<https://github.com/nerdy-pro/dxpdf>
- 系统架构：[`docs/architecture/system-overview.md`](docs/architecture/system-overview.md)
- 发布门禁：[`docs/quality/release-gates.md`](docs/quality/release-gates.md)
- 兼容性状态：[`docs/quality/compatibility-status.md`](docs/quality/compatibility-status.md)

## 贡献原则

1. 先找到第一个结构或视觉分歧，再修改布局算法。
2. 新行为必须配最小夹具、负向控制和真实文档验证。
3. 避免文件名特判、全局缩放和无证据的固定补偿量。
4. 修改后运行格式、测试、wheel 安装和相关视觉门禁。
5. 不提交私有文档、未审计字体或无许可证的二进制资产。

dxpdf-mt 的近期方向是先成为可控、可复现、便于 Python 部署的业务文档转换器，再逐步
扩大对一般 Word/WPS 文档的兼容覆盖面。
