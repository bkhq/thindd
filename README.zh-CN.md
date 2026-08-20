# thindd

[English](README.md) · **简体中文**

[![CI](https://github.com/bkhq/thindd/actions/workflows/ci.yml/badge.svg)](https://github.com/bkhq/thindd/actions/workflows/ci.yml)
[![Release](https://github.com/bkhq/thindd/actions/workflows/release.yml/badge.svg)](https://github.com/bkhq/thindd/releases)

> 精简版 `dd`：只写镜像里真正有数据的字节，中间那些零一概不写。

一个 2 GiB 的镜像，实际载荷 316 MiB，刷写耗时就等于写 316 MiB 的时间。名字取自
thin provisioning（精简置备）——不装东西的空间不该花钱。

底层是 Yocto Project 的 [`bmaptool`](https://github.com/yoctoproject/bmaptool)
所用的块映射（bmap）格式，与其**双向文件级兼容**，并加了一项关键能力：**零区消除**。
gzip 压缩的镜像会被透明解压，`.img.gz` 一步到位。

## 它解决的问题

上游 `bmaptool` 之所以快，是因为只写镜像中携带数据的块。它靠向文件系统询问镜像文件
的哪些部分有真实 extent 支撑（`SEEK_HOLE` / `FIEMAP`）来找出这些块。这对**构建时就是
稀疏文件**的镜像非常有效，但对你手头通常拿到的那种镜像完全无效：

* 下载后解压出来的 —— 解压会把每个字节都写实，包括零；
* 从设备上 `dd` 下来的；
* 构建系统输出时没有保留稀疏性的；
* 经过不带 `--sparse` 的 `tar`、`scp`，或者在 U 盘里中转过的。

这类镜像是**稠密**的：每个零字节都实打实躺在盘上。`SEEK_HOLE` 会报告 100% 已映射，
`bmaptool` 就退化成了 `dd`。

`thindd` 额外**按内容**检测全零块，所以这类镜像也能按真实载荷的速度刷写。

## 实测数据

一个 2 GiB 的稠密镜像，内含 316 MiB 真实数据，写入 NVMe 上的文件
（`--mode zero` 还会顺带清空目标，所以它多做了事）：

| | 墙钟时间 | 实际写入 | 目标占盘 |
|---|---|---|---|
| `dd bs=8M conv=fsync` | 22.7 s | 2.0 GiB | 2.1 GiB |
| 上游式 bmap（仅空洞检测） | — | 2.0 GiB（映射率 100%） | 2.1 GiB |
| `thindd copy`（**带** bmap） | **1.9 s** | 316 MiB | 317 MiB |
| `thindd copy`（**不带** bmap，边读边扫） | 13.1 s | 316 MiB | 317 MiB |
| `thindd copy --mode zero` | 3.4 s | 316 MiB + 打洞 | 317 MiB |

同一镜像用 `gzip -1` 压缩后（占盘 324 MiB）：

| | 墙钟时间 | 实际写入 | 目标占盘 |
|---|---|---|---|
| `gzip -dc image.gz \| dd bs=8M conv=fsync` | 25.2 s | 2.0 GiB | 2.1 GiB |
| `thindd copy image.gz out`（无 map） | **2.0 s** | 316 MiB | 317 MiB |
| `thindd copy image.gz out`（有 map） | 2.2 s | 316 MiB | 317 MiB |
| `thindd create image.gz` | 0.6 s | — | — |

所有输出与源镜像逐字节一致。对 loop 设备做的测试里，块层计数给出了客观佐证：写入
57344 KiB 扇区，而 `dd` 是 262144 KiB。

有两行值得展开说：

* **带 bmap**：映射表已经指明哪 316 MiB 有用，所以镜像既不用整读也不用整写。这是 CI
  或产线上想要的流程。
* **不带 bmap**：整个镜像仍然要**读**一遍（零就是这么找出来的），但只**写** 316 MiB。
  而在人们真正刷写的介质上——SD 卡、eMMC、10–40 MB/s 的 U 盘——写才是瓶颈，且要慢一个
  数量级。所以实践中这一行的表现非常接近带 bmap 那一行，而且事先什么都不用准备。

## 安装

每个打标签的版本都提供预编译二进制：

| 平台 | 归档文件 |
|---|---|
| Linux x86_64 | `thindd-<tag>-x86_64-unknown-linux-musl.tar.gz` |
| Linux arm64 | `thindd-<tag>-aarch64-unknown-linux-musl.tar.gz` |
| macOS Intel | `thindd-<tag>-x86_64-apple-darwin.tar.gz` |
| macOS Apple 芯片 | `thindd-<tag>-aarch64-apple-darwin.tar.gz` |

```bash
# Linux x86_64，最新版本
curl -fsSL https://github.com/bkhq/thindd/releases/latest/download/thindd-$(
  curl -fsSL https://api.github.com/repos/bkhq/thindd/releases/latest | grep -o '"tag_name": "[^"]*' | cut -d'"' -f4
)-x86_64-unknown-linux-musl.tar.gz | tar xz
sudo install -m755 thindd /usr/local/bin/
```

Linux 归档是静态链接 musl 的：单文件，不用对 glibc 版本，任何发行版都能跑。每个归档
旁边都附带 `.sha256` 校验文件。

从源码安装：

```bash
cargo install --git https://github.com/bkhq/thindd thindd
# 或者在克隆目录里：
cargo build --release          # target/release/thindd
```

没有 C 依赖，没有 `openssl`，没有 `ioctl`，没有 `unsafe`。空洞检测用
`lseek(SEEK_DATA/SEEK_HOLE)`，快速清零用 `fallocate`，gzip 用
[`zlib-rs`](https://crates.io/crates/zlib-rs) —— 全都通过安全封装调用。

## 使用

```bash
# 刷写镜像。同目录下的 <IMAGE>.bmap 会被自动识别。
thindd copy core-image.wic /dev/sdb

# 没有 bmap 文件？照样快 —— 零区在读取过程中被识别出来。
thindd copy --no-bmap core-image.wic /dev/sdb

# 顺便把不属于镜像的部分清掉，而不是留着设备上的旧数据。
# 走的是 fallocate/BLKZEROOUT，几乎不花代价。
thindd copy --mode zero core-image.wic /dev/sdb

# 预先生成映射表，之后刷写连零都不用读。
thindd create core-image.wic            # 生成 core-image.wic.bmap
thindd info core-image.wic.bmap --ranges

# 压缩镜像边解码边刷 —— 不落临时文件，不用搭管道。
thindd copy core-image.wic.gz /dev/sdb
thindd create core-image.wic.gz         # 生成 core-image.wic.bmap

# 流式输入同样可用，压不压缩都行。
zstd -dc core-image.wic.zst | thindd copy --no-bmap - /dev/sdb
cat core-image.wic.gz | thindd copy --no-bmap - /dev/sdb
```

### `copy`

| 选项 | 默认值 | 含义 |
|---|---|---|
| `--bmap FILE` | 存在则用 `<IMAGE>.bmap` | 使用指定的映射表 |
| `--no-bmap` | | 忽略映射表，全靠扫描发现 |
| `--detect holes\|zeros\|both\|none` | `both` | 哪些内容可以跳过 |
| `--mode skip\|zero` | `skip` | 跳过的区域如何处理 |
| `--seek BYTES` | `0` | 在目标的这个字节偏移处开始写入镜像 |
| `--zap` | 关 | 只清设备两端各 4 MiB —— 分区表所在的地方，很快 |
| `--wipe` | 关 | 先清空整个目标，包括镜像末尾之后 |
| `--decompress auto\|none\|gzip` | `auto` | 透明解压 |
| `--verify` | 关 | 写完后把目标读回来与镜像比对 |
| `--no-verify` | 关 | 跳过映射表里的分段校验和 |
| `--no-sync` | 关 | 退出前不做 `fsync` |
| `--force` | 关 | 内核报告设备忙时仍然写入 |
| `--bs BYTES` | `8M` | 每次读和每次写的大小 —— 相当于 `dd` 的 `bs=` |
| `--sync-every BYTES` | `16M` | 每写入这么多就刷一次盘；`0` 表示不刷 |
| `--queue-depth N` | `4` | 读写线程之间在途的批次数 |

`--detect holes` 精确复现上游 `bmaptool` 的行为，方便做同口径对比。

### `create`

| 选项 | 默认值 | 含义 |
|---|---|---|
| `-o, --output FILE` | `<IMAGE>.bmap` | 输出位置（`-` 表示标准输出） |
| `--detect holes\|zeros\|both\|none` | `both` | 什么算可跳过 |
| `--checksum sha1\|sha256\|sha512\|none` | `sha256` | 分段摘要算法 |
| `--decompress auto\|none\|gzip` | `auto` | 为解压后的镜像建表 |
| `--block-size BYTES` | 文件系统首选值 | 映射粒度 |
| `--bs BYTES` | `8M` | 每次读取的大小 |

`--detect holes --checksum none` 完全不用读取内容：映射表直接来自 `SEEK_HOLE`。

## 写入指定偏移

`--seek` 把镜像写到目标的非零起始位置 —— 相当于 `dd` 的 `seek=`，只不过单位是字节而不是块，
所以 `--seek 8K` 就是 8192。

```bash
# 把 bootloader 写到 SoC 的 ROM 会去找的那个偏移
thindd copy --seek 8K u-boot-sunxi-with-spl.bin /dev/sdb

# 在一套手工排布的更大布局里更新系统镜像，后面的数据分区保持不动
thindd copy --seek 32M --mode zero system.img /dev/sdb
```

非零偏移会把这次拷贝变成一次**局部更新**，其余行为都由此推导：

* 普通文件目标不够长会被扩展，但**绝不会被截断** —— 镜像之后的内容原样保留；
* 容量检查会把偏移算进去，失败时也把话说清楚：*image is 256.0 MiB written at offset
  300.0 MiB, needing 556.0 MiB, but destination only holds 512.0 MiB*；
* `--mode zero` 清零的是**镜像自身范围内**的空隙，而这个范围现在从偏移处开始 —— 它依然
  不会越界；
* bmap 不受影响。它描述的是镜像本身，而不是镜像要去哪里，所以同一份映射表在任何偏移都能用。

`--wipe` 的含义仍然是整块设备，所以和 `--seek` 一起用等于"先清空整卡，再把镜像写到偏移处"
—— 手工排布一张新卡时这正是你要的，其他场合则多半是误用。

## 压缩镜像

`--decompress auto`（默认）按**魔数**而非文件名判断输入类型，所以 gzip 流不管叫
`.gz`、`.img` 还是从标准输入进来都能识别。`--decompress none` 强制把看起来像压缩的
文件当裸数据读；`--decompress gzip` 强制对没有可辨识文件头的流启用解码器。多 member
的流（`pigz`、`cat a.gz b.gz`、rsyncable gzip）都能处理。

有两点后果需要知道：

* 压缩流无法回退，所以**整个镜像必然要完整解压一遍**，包括那些最终会被跳过的部分 ——
  这些字节得先被还原出来，才能判断它们是零。节省完全发生在写入侧，而真实介质上时间
  本来就花在写上。
* 因为不能定位，空洞检测对压缩镜像不适用，全部工作由零扫描完成。这并没有损失什么 ——
  压缩文件本来就没有空洞可找。

映射表查找会跟随压缩后缀：`thindd copy core-image.wic.gz` 会依次寻找
`core-image.wic.gz.bmap` 和 `core-image.wic.bmap`；对压缩镜像执行 `create` 时写出的
是后者，因为无论哪种方式，映射表描述的都是**解压后**的镜像。

解码使用 [`zlib-rs`](https://crates.io/crates/zlib-rs)，即 zlib 的 Rust 重写版 ——
没有 C，没有 `*-sys`。它位于 `thindd-core` 的 `gzip` feature 之后，默认开启；
`--no-default-features` 会同时移除代码和依赖，届时遇到压缩镜像会明确报错，而不是把
压缩数据当作镜像写进设备。

## 分区这个概念并不存在

`thindd` 根本不知道什么是分区。它对每个块只判断一件事：里面是不是每个字节都是零？不是就写。
分区表、文件系统、镜像的任何结构信息，它一概不参考。

这对**分区之外**的内容很要紧 —— 而在嵌入式镜像里，有意思的部分大多恰好在分区之外：扇区 0
的保护性 MBR、扇区 64 的引导块、扇区 16384 的二级 loader、保留区里的厂商密钥。这些全都是
非零数据，所以全都会被写入，和 `dd` 的结果一样。会丢掉这些东西的是「按分区理解镜像」的那类
工具，而本工具不是。

判断粒度是块（默认 4096 字节），所以哪怕只有一个非零字节，也会带着它整个块一起写。零的海洋
里一个孤零零的厂商标记不会丢。

反过来的情况也值得知道：如果**镜像里**是零、而**设备上**那个位置有你需要保留的东西 ——
Rockchip 的 vendor storage（存序列号和 MAC 地址）是典型例子 —— 那么默认的 `--mode skip`
会保住它，而 `--mode zero` 或 `--wipe` 会抹掉它。这是唯一一种「看起来更谨慎的选项反而是破坏
性的」情形。

## `skip` 与 `zero` —— 唯一需要你做决定的地方

bmap 的契约（继承自上游）是：**"被映射的块会被写入"**，对其余部分不做任何承诺。对于
空白设备，或者新建文件（结果是稀疏文件，读出来就是零），这正好合适，且不花任何代价。

但如果设备上**已经有数据**，`--mode skip` 会把旧字节留在空隙里。这是上游行为，
也仍然是默认值，但重新刷写时这往往不是你想要的：

```bash
thindd copy --mode zero core-image.wic /dev/sdb
```

`zero` 会请内核用 `fallocate` 来清零：普通文件用 `FALLOC_FL_PUNCH_HOLE`（不产生 I/O，
也不占磁盘空间），块设备用 `FALLOC_FL_ZERO_RANGE` —— 多数 SSD/eMMC/SD 控制器会在内部
执行，而不是把零通过总线写过去。只有硬件（或平台）两者都不支持时，才退回到手写零页。

### 到底会留下什么

用一块 256 MiB 的 loop 设备预先填满随机数据，再刷入一个 64 MiB、内含 16 MiB 载荷的镜像，
实测结果：

| | 镜像范围内的空隙 | 镜像末尾之后 |
|---|---|---|
| 空白设备，任何模式 | 零 | 零 |
| 有数据的设备，`--mode skip` | **旧字节保留** | 旧字节保留 |
| 有数据的设备，`--mode zero` | 已清零 | **旧字节保留** |
| 有数据的设备，`--wipe` | 已清零 | 已清零 |

上游 `bmaptool` 只有第一种行为，且没有选项可改：它的 `copy()` 只写映射批次，别的什么都不做。

由此有两点结论。

**镜像范围内的残留通常无害，但不总是。** 它们落在新文件系统眼中的空闲空间里，不会被当作
文件内容读出来，设备照常启动。真正要紧的是两种场合：设备脱离你的掌控时（旧密钥、日志用
常规取证手段就能恢复），以及你想通过读回设备与镜像比对来验证刷写时（对不上）。

**任何 `--mode` 取值都够不到镜像之外。** bmap 描述的是镜像，而镜像对它之后的空间只字未提。
如果这块设备之前是另一套分区布局，残留的 GPT 备份头或旧文件系统超级块会留在那里，可能让
`blkid`、udev 或引导器找到一个已经不存在的分区 —— 这正是刷完之后起不来的常见原因。

有两个选项能解决，而通常该用便宜的那个：

```bash
thindd copy --zap  --mode zero core-image.wic /dev/sdb   # 毫秒级
thindd copy --wipe             core-image.wic /dev/sdb   # 整个设备
```

`--zap` 清掉设备**两端各 4 MiB**。问题就出在那里：MBR 是第一个扇区，GPT 备份头是最后
33 个扇区，文件系统超级块在开头几 KB 之内。中间的部分原样保留，所以代价不随卡的容量增长
—— 无论是 512 MiB 的 loop 设备还是 512 GB 的 SSD，都是 8 MiB。

`--wipe` 则是全清。在 Linux 上这是一次覆盖全盘的 `fallocate(ZERO_RANGE)`，支持 write-zeroes
或 discard 的控制器会在内部执行，512 MiB 的设备上约 20 ms。而在内核没有这种调用的平台上
（macOS 就是），它会逐字节写零，代价随卡的容量而不是镜像的大小增长。

关于"为什么不直接格式化"：快速格式化**确实**快，而这恰恰是问题所在 —— 它只写一张新的分区表
和文件系统元数据，其余每个字节原封不动。它能解决"旧布局残留"这一半，对"旧数据残留"那一半
毫无作用。`--zap` 是同一个想法的诚实版本，另一半交给 `--mode zero`。

### 该用哪个

`--mode zero` 和 `--wipe` 不是互相的替代品，它们回答的是不同问题：*让镜像那一段与镜像一致、
其余不碰* 对 *让整块设备干净*。

| | |
|---|---|
| 空白或全新介质 | 默认值 —— 最快，也没什么可清 |
| 镜像区域要干净，但保留设备上镜像之后的内容 | `--mode zero` |
| 整块设备要干净 —— 换分区布局、清残留 GPT 备份头、设备要转手 | `--wipe` |

两点区别。

**`--wipe` 会销毁设备上的一切，不只是镜像那一段。** 如果卡上在系统镜像之后还有一个用户数据
分区（很常见的安排），`--wipe` 会连它一起抹掉，`--mode zero` 不会。

**两者的代价随不同的东西增长。** `--mode zero` 的上界是镜像大小，`--wipe` 是设备容量。在一块
8 GiB 的目标上刷 512 MiB、载荷 64 MiB 的镜像，按块层计数实测：

| | 写入扇区 |
|---|---|
| 默认 | 64 MiB |
| `--mode zero` | 512 MiB |
| `--wipe` | 8256 MiB |

在实现了 write-zeroes 或 discard 的控制器上，三者墙钟时间差不多，因为清零根本不过总线。在没有
实现的硬件上，这一列就是真实写入时间：一根 15 MB/s 的 U 盘上，这三次分别约为 5 秒、35 秒、
9 分钟。卡相对镜像越大，差距越悬殊。

`--mode zero` **不是**默认值，这是刻意的。只有当目标实现了 write-zeroes 或 discard 时
它才近乎免费。不支持的时候 —— 普通 USB 大容量存储就是典型 —— `fallocate` 会失败，退回去真的
写零：一个 2 GiB、载荷 316 MiB 的镜像意味着要写 1.7 GiB 的零，在 15 MB/s 的 U 盘上把 20 秒的
刷写变成两分钟。一个代价在"几乎为零"和"和 `dd` 一样慢"之间随硬件摇摆的默认值，对一个卖点就是
可预测速度的工具来说是错的。要用就显式指定。

## 安全性

* 块设备以 `O_EXCL` 打开。只要设备本身或它的任何一个分区处于挂载或被占用状态，内核就
  会拒绝，所以你不会因为设备名手滑而覆盖掉正在运行的根文件系统。`--force` 可以放弃这
  层保护。
* 写入任何内容之前，先校验目标容量是否装得下镜像。
* bmap 里的分段校验和在拷贝**过程中**验证，因此损坏的镜像或对不上的 bmap 会中止操作，
  而不是产出一个坏设备。
* 拷贝结束后，还会核对实际读取的映射块数与 bmap 声明的数量 —— 即使每个分段校验和都碰
  巧通过，属于另一个镜像的 bmap 也会被抓出来。
* `--verify` 会把目标读回来与镜像比对。bmap 的校验和验证的是**读进来**的镜像；这是唯一
  覆盖「真正写进设备的东西」的检查。刷完的卡行为不对时，第一个该用的就是它 —— 它会告诉你
  两者第一次不一致的字节偏移。
* 对块设备，默认每写入 16 MiB 刷一次盘，这样中断一次慢速 U 盘的刷写，不会让你在
  `close()` 里干等好几分钟。

## 平台支持

| | Linux | macOS |
|---|---|---|
| copy / create / info / gzip / 校验和 | 支持 | 支持 |
| 空洞检测（`SEEK_HOLE`/`SEEK_DATA`） | 支持 | 调用可用，但见下 |
| 块设备 `O_EXCL` 保护 | 支持 | 支持 |
| `--mode zero` 不实际写零 | `fallocate` | 退回到显式写零 |
| I/O 调度器 / 回写调优 | `sysfs` | 无对应旋钮 |
| 页缓存提示 | `posix_fadvise` | 交给内核自身的启发式 |

所有平台相关代码都在 `cfg` 之后，CI 会在两个平台上分别构建、跑 clippy 并执行完整测试
套件。在此之上，Linux 侧还有两套：

* [`tests/blockdev/`](tests/blockdev/) 在特权容器里操作 loop 设备 —— `O_EXCL` 保护、
  容量检查、sysfs 调优、`fallocate` 清零 —— 并在块层确认确实只有被映射的扇区被写入；
* [`tests/vm/`](tests/vm/) 启动一台带自有 virtio 磁盘的 QEMU 客户机，覆盖前两者都够不到
  的场景：写入**分区** —— 分区自己没有 I/O 旋钮，必须向上走到父盘。

**macOS 侧这些都没有覆盖** —— macOS 二进制经过构建和单元测试，但没有经过实机验证。

macOS 上有三点需要留意：

* 应该用 `/dev/rdiskN`（先 `diskutil unmountDisk /dev/diskN`）—— 它是不经缓冲的路径，
  比 `/dev/diskN` 快得多。它是**字符设备**而不是块设备，`thindd` 对此做了处理：容量通过
  `lseek` 获取，所以容量检查、`--wipe` 和结束时的 sync 在它上面都正常工作。剩下一个注意
  事项：裸设备要求每次写入都是其块大小的整数倍，而 `thindd` 不会为最后那个不足一块的写入
  补齐，所以镜像大小不是扇区整数倍时，最后一次写入可能失败。磁盘镜像几乎总是整数倍；万一
  不是，改用 `/dev/diskN`。
* `--mode zero` 和 `--wipe` 在那里是诚实但慢的：没有 `fallocate`，零会被真的写出去，所以
  整盘 `--wipe` 的代价等同于写满整个设备。`thindd` 会在开始前把这一点说明白。
* 单用 `--detect holes` 可能把镜像映射成 100%。`SEEK_HOLE` 本身是实现了的，但 APFS 对
  按常规方式写出的文件通常不留空洞，于是它无洞可报 —— CI 上看到的正是这种情况。默认的
  `--detect both` 不受影响：零扫描会按内容找出同样的区域，而这正是本工具存在的意义。

不支持 Windows，也没有计划 —— 整个设计建立在 Unix 的定位写入和稀疏文件语义之上。

## 格式兼容性

bmap 2.0 XML 格式的读写均已实现，1.x 版本也能读。这一点是**实测**出来的，不是假设：

* 本工具生成的映射表能被上游 `bmaptool` 解析，其自校验和与每一个分段校验和均通过验证，
  产出的镜像逐字节一致；
* 上游 `BmapCreate` 生成的映射表能被本工具解析并正确拷贝。

`thindd info` 对两者都能给出说明。

## 工作原理

```
 读线程                                 写线程
 ──────                                 ──────
 从缓冲池取一块  ──┐
 读取一个批次      │  有界
 区分零区/数据区   │  通道    ──▶  对数据段执行 pwrite
 顺带计算校验和    │               对零区执行 fallocate
 发送批次        ──┘               把缓冲归还池中
```

读和写是重叠进行的，因为源端（NVMe 或页缓存）和目标端（SD 卡）的速度通常差一个数量级。
缓冲通过第二条通道回收，所以拷贝 32 GiB 镜像和拷贝 32 MiB 镜像占用同样固定的 32 MiB
内存。

零检测是整个工具的热循环 —— 镜像的每一个字节都要过它。它用切片与一页静态零做比较，而
不是逐字节遍历，因为 `[u8]` 的切片相等会降级为 `memcmp`：libc 里是向量化实现，且遇到
第一个不同的字节就返回。一个整批全零的缓冲（最常见的情况）一次调用就能判定。它跑在内存
带宽上，且不需要 `unsafe`。

### crate 划分

| crate | 内容 |
|---|---|
| [`thindd-core`](crates/thindd-core) | 格式、空洞/零检测、解压、拷贝引擎 —— 可作为库使用 |
| [`thindd`](crates/thindd) | 命令行 |

库的用法示例：

```rust
use thindd_core::{
    Destination, ImageSource,
    copy::{self, CopyOptions},
    create::{self, CreateOptions},
    progress::NoProgress,
};
use std::path::Path;

fn flash() -> Result<(), thindd_core::Error> {
    let image = Path::new("core-image.wic");
    let bmap = create::create(image, &CreateOptions::default(), &NoProgress)?;
    bmap.write_to(Path::new("core-image.wic.bmap"))?;

    let dest = Destination::open(Path::new("/dev/sdb"), false)?;
    let stats = copy::copy(
        ImageSource::open(image)?,
        &dest,
        Some(&bmap),
        &CopyOptions::default(),
        &NoProgress,
    )?;
    println!("skipped {:.1}%", stats.elided_percent());
    Ok(())
}
```

## 尚未实现

刻意划在当前范围之外；上游有这些能力，且都是可叠加的：

* `.xz` / `.zst` / `.bz2` 解压 —— `.gz` 已支持，其余的先用对应解压器管道输出，再从 `-`
  读入；
* 直接从 URL 读取镜像；
* bmap 文件的 GPG 签名校验；
* `psplash` 进度管道。

## 开发

```bash
just ci     # fmt + clippy + nextest + doctest + cargo-deny + cargo-shear
```

有两套测试超出了 `cargo test` 的能力范围，都在特权容器里针对自己创建的设备运行：

* [`tests/blockdev/`](tests/blockdev/) —— loop 设备：`O_EXCL`、容量检查、sysfs 调优、
  `fallocate` 清零，以及在块层计数证明只写了被映射的扇区；
* [`tests/vm/`](tests/vm/) —— 一台带自有 virtio 磁盘的 QEMU 客户机，用于测试写入分区。

发布版本通过推送标签触发：

```bash
git tag -a v0.1.0 -m "v0.1.0" && git push origin v0.1.0
```

本工作区遵循 PMA-Rust 基线：edition 2024、每个 crate 都有 `#![forbid(unsafe_code)]`、
deny-warnings 策略写在 `[workspace.lints]` 而不是 CI 命令行里、测试之外不出现
`unwrap`/`expect`/`panic!`。

## 许可

GPL-2.0-only。`thindd` 是独立实现，但它读写的 bmap 文件格式 —— 以及为保持兼容而必须匹配
的语义 —— 来自 Yocto Project 的 `bmaptool`（Copyright (c) 2012-2014 Intel, Inc.，
GPLv2）。许可证保持一致，以便两者可以无摩擦地配合使用。
