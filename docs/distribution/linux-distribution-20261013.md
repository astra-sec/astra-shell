# Astra CLI / astrad 分发准备（2026-10-13）

状态：方案与隔离可行性验证，未公开发布。对象是 Rust `astra`、`astrad`，不是 Apple App 版本号。服务端基线 f882354 + SESS-04 工作区；取证位于 `../astra-apple/.build/butler-stability-20261013/`。所有 `<...>` 及 `example.invalid` 都是未配置的占位符。

## 推荐路线

首发采用签名自建 APT 仓库，首先验证 Ubuntu 22.04/24.04 LTS 的 amd64，加入 arm64 原生构建后才承诺支持。26.04 LTS 单独跑矩阵，不能由“都是 Linux”推断；当前生产实测 Rome 为24.04 amd64、Icelake为22.04且也是 managed。Ubuntu 生命周期以[官方周期表](https://ubuntu.com/about/release-cycle)为准。crates.io 并行准备，依赖拆分完成后提供源码安装。PPA 和官方归档留为后续渠道。

| 渠道 | 本项目成本与限制 | 建议 |
|---|---|---|
| 签名自建 APT | 自管构建、托管、Release签名、过期刷新、回滚和密钥轮换；可以固定Rust工具链和完整审计后的vendored依赖，不依赖目标Ubuntu自带Rust版本 | 首发；先在隔离容器/VM测试再发布 |
| Launchpad PPA | 上传经OpenPGP签名的Debian源码包，Launchpad按发行版/架构构建；需要账号/签名和可离线构建的完整源码依赖，不能只上传本机预编译.deb | 当前Rust依赖链整理后再做；PPA并非Ubuntu官方支持承诺 |
| Ubuntu 官方归档 | 需要正式Debian/Ubuntu打包维护、依赖/版权核查、审核/赞助与发行周期维护；对现有多层未发布fork成本最高 | 不作为本轮首发前置 |

PPA流程根据[Ubuntu官方上传指南](https://ubuntu.com/project/docs/contributors/new-package/upload-packages-to-a-ppa/)；这里的渠道选择为结合仓库的工程判断。

## crates.io：实际阻塞

2026-10-13 通过公开 `/api/v1/crates/<name>` 与 `/owners` 查询（未读取token）：

| 包名 | 结果 |
|---|---|
| astra-shell | HTTP404，当前无已发布记录；不等于为用户保留了名称 |
| astra | 已存在0.4.0，repository `ibraheemdev/astra`，owner `ibraheemdev` |
| astrad | 已存在0.1.1-rc1，repository `SparkAIUR/astra`，owner `trisongz` |
| astra-wezterm-term / astra-wezterm-cell / astra-wezterm-char-props / astra-wezterm-escape-parser / astra-wezterm-surface / astra-finl-unicode | 查询均404，拟议名仍须发布前复核 |
| finl_unicode | 已存在1.5.0，owner `dahosek`；不代表等价于本仓使用的wez fork |

证据 `crates-public-query.json`。保留主包名 `astra-shell`，用户命令仍可叫 `astra`、`astrad`；crate名和binary名不要求相同。不要申请/覆盖他人包名。[Cargo发布规则](https://doc.rust-lang.org/cargo/reference/publishing.html)说明名称先到先得、发布版本不可覆盖，发布前必须检查metadata和包文件。

递归链（本地 `cargo metadata --offline` 与各 Cargo.toml 取证）：

```text
astra-shell 0.1.0
└─ astra-wezterm-term 0.1.0 (publish=false)
   ├─ wezterm-cell 0.1.0 (publish=false)
   │  ├─ wezterm-char-props 0.1.3 (publish=false)
   │  ├─ wezterm-escape-parser 0.1.0 (publish=false)
   │  └─ finl_unicode 1.3.0 (Git fork, pinned SHA)
   ├─ wezterm-escape-parser 0.1.0
   ├─ wezterm-surface 0.1.0 (publish=false)
   │  ├─ wezterm-cell / wezterm-char-props / wezterm-escape-parser
   │  └─ finl_unicode Git fork
   └─ finl_unicode Git fork
```

Git SHA为 `a1892f26245529f2ef3877a9ebd610c96cec07a6`。其余如termwiz 0.23.3、wezterm-bidi 0.2.3、wezterm-dynamic 0.2.1已有registry版本。`dependency-audit.json`记录完整路径与source。

隔离复制tracked源码（含本轮工作区内容，不含.git/target/state/凭证），独立CARGO_HOME执行：

```sh
cargo package --allow-dirty --list
cargo publish --allow-dirty --dry-run
```

前者成功列清单并报告metadata缺项；后者失败：`dependency astra-wezterm-term does not specify a version`。没有上传。**仅补version仍不够**：五个本地crate禁止publish，内部路径依赖也缺version；finl_unicode的git+version在发布时去掉git，可能改用不同代码，不能静默转换。[Cargo依赖规则](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html)要求发布依赖可由registry解析；path/git+version是开发与registry双来源，不会把本地依赖内嵌进主crate。

最小推荐修复顺序（下一轮实施）：

1. 审核finl fork与registry源码差异。若不同，按保留原源码/许可证的方式发布 `astra-finl-unicode`，依赖别名维持Rust导入名：`finl_unicode = { package = "astra-finl-unicode", version = "=<FORK_VERSION>" ... }`。没有等价证明前不替换引擎或Unicode实现。
2. 发布无本地依赖的 `astra-wezterm-char-props`、`astra-wezterm-escape-parser`，再cell，再surface，再astra-wezterm-term，最后astra-shell。每条本地边用 `package` + `path` + 精确 `version`，保留library导入名。
3. 逐crate补metadata/许可证、改publish配置；依赖发布可查询后再打包上层。使用临时registry或隔离发布工作区验证依赖拓扑，不用修改Cargo.lock假装registry已有版本。
4. 将所有引擎源码合并为单crate是替代方案，但涉及模块/API重构，风险高于依赖发布；`cargo install --git`能作为开发安装，不能解决crates.io发布要求。

## 元数据、许可、MSRV与可重现性

主包当前version=0.1.0、edition=2024、license=`MIT OR Apache-2.0`，有description；缺rust-version、repository、homepage、显式readme/include与release记录。Apple 1.1不应直接替代Rust包0.1.0。建议首个试用版 `<0.1.0-rc.1>`，正式版本由维护者决定，不能当作已存在版本。

主仓tracked文件未找到根LICENSE全文，只有term fork的 `LICENSE.md`。fork声明MIT；finl_unicode声明 `(MIT OR Apache-2.0) AND Unicode-DFS-2016`。发布前补主仓双许可证全文、作者与版权确认、每个镜像源/修改说明及Unicode数据许可；生成全锁文件依赖SBOM/attribution并人工核查。这里没有证明第三方许可审核已经闭合。

本轮实际构建工具链Rust/Cargo 1.98.0。锁定依赖的最高已声明rust-version为1.88（ICU/rcgen/time等）；这只是下界证据，不是本项目MSRV。下一轮先用1.98.0作为受支持构建工具链，再以Linux/macOS的`cargo +1.88.0 check/test --locked`等矩阵验证可否声明更低版本。`rust-version`用于明确最低支持而不能取代测试，见[Cargo MSRV说明](https://doc.rust-lang.org/cargo/reference/rust-version.html)。22.04/24.04不依赖系统预装Rust能编译edition2024；构建镜像固定工具链摘要，终端用户APT安装无需Rust。

建议主包include：`src/**`, `proto/**`, `Cargo.toml`, `Cargo.lock`, `README.md`, `LICENSE-*`, `THIRD_PARTY_NOTICES*`, 必要contrib文档。每个子crate显式列src/build.rs/数据表/LICENSE。发布清单要扫描秘密、绝对开发路径、测试状态和大文件；当前`cargo-package-list.log`是真实清单，并非最终审定清单。Rust protocol.rs是手写prost模型，构建不依赖protoc；仍纳入schema同步检查。

构建CI固定源码tag+lock、rustc、Ubuntu镜像摘要、目标三元组、SOURCE_DATE_EPOCH和remap-path-prefix，离线vendor构建两次并比较散列，差异用diffoscope定位。现有`--locked`通过不意味着位级可重现，当前未做双构建证明。

安装命令草案（版本和crate尚未发布）：

```sh
cargo install astra-shell --version '<RUST_VERSION>' --locked  # 默认安装astra和astrad
# 仅客户端
cargo install astra-shell --version '<RUST_VERSION>' --locked --bin astra
```

Cargo会编译源码，二进制进入Cargo bin目录，需C编译工具链等依赖，可能耗时；不会替用户初始化证书、授权或systemd。详见[Cargo install官方说明](https://doc.rust-lang.org/cargo/commands/cargo-install.html)。避免`sudo cargo install`混入root HOME。安装后rootless草案：

```sh
install -d -m 700 "$HOME/.local/state/astra"
astrad init --state-dir "$HOME/.local/state/astra"
# 操作者将经过核对的公钥写入该目录authorized_keys（0600），不上传私钥。
astrad serve --listen 127.0.0.1:4433 --state-dir "$HOME/.local/state/astra" --session-root "$HOME"
# 完成认证/防火墙配置后，操作者再选择监听地址和是否启用user unit。
```

## .deb拆分及路径

建议逻辑包：`astra-shell-cli`提供 `/usr/bin/astra`；`astra-shell-server`提供unit/配置模板/运维文档；versioned runtime提供astrad。最终包名须查Ubuntu/Debian索引，当前未创建这些包。

- system配置 `/etc/astra/`（root-owned conffiles）、managed状态 `/var/lib/astra/`（0700，用户子目录沿用服务器UID隔离）、runtime socket由现有server state路径派生。现有 `/srv/astra` 不自动迁移。
- rootless用用户自有state目录、known_hosts继续`~/.config/astra/known_hosts`。包安装不得遍历/清除每位用户HOME。
- managed通过root gateway切换到真实Unix用户，不能用一个服务用户替代每UID worker。不能盲目开启NoNewPrivileges/PrivateTmp使sudo或用户/tmp语义改变。
- unit默认不enable/start；不自动开放公网4433或复制authorized_keys。`KillMode=process`保留managed worker，但不解决内部协议代际兼容。rootless单进程持有PTY，每次重启终止其Shell。
- Debian包配置变更遵守conffiles/管理员修改保留规则，参考[Debian文件与配置策略](https://www.debian.org/doc/debian-policy/ch-files.html#configuration-files)。用户证书/私钥和持久catalog不是随版本覆盖的package payload。

## 升级不静默终止PTY：首发必须实现的策略

WORK-03未完成。本轮SESS-04修改worker行为，新gateway复用旧worker不会自动获得恢复能力；旧worker对未知内部能力会拒绝，不能以重启gateway宣称升级完成。

**默认仅安装新制品，绝不自动restart/stop gateway或worker。** maintainer scripts幂等、可非交互执行，不用脚本弹窗索取全局杀会话确认。借助debhelper的no-start/no-stop-on-upgrade设置并检查最终生成脚本；`daemon-reload`可以执行但不能隐含restart。[Debian maintainer-script规则](https://www.debian.org/doc/debian-policy/ch-maintainerscripts.html)要求幂等和正确处理install/upgrade/remove/failed-upgrade路径。

另一个容易漏掉的风险：仅覆盖`/usr/bin/astrad`但不重启，运行gateway的`current_exe()`可能指向已删除的旧文件，后续新建worker失败。首发建议versioned runtime包可并存：例如 `astra-shell-runtime-0-1-0-1` 安装 `/usr/lib/astra-shell/0.1.0-1/astrad`；unit选择明确版本路径，更新unit后只daemon-reload。当前仍运行的gateway及worker所用旧runtime必须保留（标记manual，维护工具清理前核查/proc/exe、worker身份和所有UID）。这套策略需下一轮真实deb升级测试；未实现前不要宣称无中断APT升级。

操作者单独选择维护操作：

1. 只装新包、稍后维护：旧gateway/worker继续服务；状态页/命令提示“已安装≠已运行”。
2. 兼容且已证明的gateway-only切换：验证旧worker内部协议能力，允许时才重启gateway，逐UID确认PTY存活。
3. 本轮SESS-04全worker升级：先枚举所有UID/Terminal/worker exe，展示将终止的Shell，用户批准维护窗后逐个校验PID再TERM；新worker验证后移除旧runtime。不能重启cve2env所在服务或使用广泛pkill。

`apt remove`：首发保守设计在活跃gateway/worker存在时明确拒绝服务包/对应runtime的remove，要求操作者先执行维护流程；不能默杀Shell，也不能留下会因缺文件而不能新建worker的半运行状态。`purge`在无活跃进程时移除包管理的conffiles与unit；默认保留用户创建的证书/状态，独立显式清除命令在备份后操作。文档要明确purge保留的数据位置，不让用户以为全部凭证已删。

回滚：签名仓库保留至少前两个制品和源码；记录旧unit精确runtime路径、包版本、schema/worker兼容矩阵、证书摘要。APT降级不会恢复被杀PTY；catalog若发生不可逆迁移，应先停止并按备份恢复，不在活跃state上直接覆盖。当前SESS-04恢复记录仅存worker内存，无磁盘schema迁移，维护重启仍销毁PTY。

## 自建仓库安装草案

以下域名/版本/指纹均未存在，**不能直接执行即视为可安装**：

```sh
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSLo /tmp/astra-release.asc https://packages.example.invalid/astra-release.asc
gpg --show-keys --with-fingerprint /tmp/astra-release.asc
# 与独立渠道公布的 <EXPECTED_FINGERPRINT> 核对后：
sudo install -m 0644 /tmp/astra-release.asc /etc/apt/keyrings/astra.asc
sudo tee /etc/apt/sources.list.d/astra.sources >/dev/null <<'SOURCES'
Types: deb
URIs: https://packages.example.invalid/ubuntu
Suites: <jammy-or-noble>
Components: main
Architectures: <amd64-or-arm64>
Signed-By: /etc/apt/keyrings/astra.asc
SOURCES
sudo apt update
sudo apt install astra-shell-cli='<DEB_VERSION>' astra-shell-server='<DEB_VERSION>'
# 安装后按上一节完成state/authorized_keys/监听和维护选择，再显式启动。
```

不用apt-key或trusted=yes。第三方源应使用独立keyring及scoped Signed-By，参见[Ubuntu完整性文档](https://documentation.ubuntu.com/security/software-integrity/archive-verification/)和[APT sources.list手册](https://manpages.debian.org/unstable/apt/sources.list.5.en.html)。仓库签InRelease/Release.gpg，Packages含包散列、设置Valid-Until并定期刷新；私钥留离线或受限签名服务，CI仅上传待签manifest，通过环境保护审批放行。先发布新公钥与旧钥交叉覆盖窗口，再轮换签名，保留吊销/过期故障演练。

版本草案：Rust `<0.1.0-rc.1>`→Debian `0.1.0~rc.1-1astra1~ubuntu22.04`；稳定 `0.1.0-1astra1~ubuntu22.04`，不同suite独立产物，避免beta排序高于正式版。不要以Apple Build号代替Debian版本。

## 现有手工安装迁移与CI门禁

Rome当前 `/usr/local/sbin/astrad`、`/home/mimi/astra-shell/state`；Icelake实际 `/usr/local/sbin/astrad`、`/srv/astra`，两者都是system managed服务。旧rootless transient说明不能作为部署命令。先读systemd ExecStart、state权限/证书摘要、所有worker版本/终端数，离线备份并校验state；新包路径不覆盖/usr/local，unit drop-in在维护窗显式切换。`PATH`可能先命中/usr/local或~/.cargo/bin，用`command -v`和散列排查，不擅自删除旧安装。证书、authorized_keys与UID目录权限保持，迁移失败恢复旧unit/原state，备份目录0700。

CI下一轮步骤：

1. 补LICENSE/metadata/MSRV、依赖发布拓扑与最终include清单，freeze lock/tag；公开名称再次查询。
2. Ubuntu22.04/24.04×amd64/arm64原生隔离runner：编译test/clippy、真实rootless/managed、SESS-04与N/N-1、Files/forward；toolchain镜像固定。若交叉编译只能证明编译，不能取代arm64运行验证。
3. 生成源码包、SBOM、第三方许可、.deb；`dpkg-shlibdeps`生成动态依赖、lintian、离线sbuild、piuparts/autopkgtest。分别安装、升级、降级、remove、purge、磁盘满、非交互、双UID长期PTY；核对旧runtime文件不被删、包脚本不发TERM、不重置证书。
4. 两次重构建比对，产出SHA256与来源证明；staging APT执行Signed-By、过期/错误签名拒绝、版本pin和回滚；不接触生产服务。
5. 用户确认账号/许可证/维护者名称、crate owner团队、仓库域名与托管、签名私钥方式、首发Ubuntu架构、版本与维护窗口后再正式发布。

本轮未cargo publish、未上传PPA、未部署APT源、未构造可安装.deb；只完成公开查询、依赖审计、真实隔离package/list/dry-run。所有失败日志保留。必要账号凭证应进入受保护存储或CI secret，不粘贴到聊天。
