# custom-branding

这个客户端的产品身份集中在这里，而不是散落在 Rust 源码、语言包和发布脚本里。
目的：换品牌、或同一客户端做第二个部署，改的是**数据**而不是满仓库找字符串；
上游合并时也不必去和解一次散落的重命名。

## 现状（**第一步，尚未接线**）

```
custom-branding/
├── config/branding.json   ← 产品身份的唯一声明处
├── branding.py            ← 读取与校验
└── test_branding.py       ← 校验规则的夹具测试
```

**目前除本模块与其测试外，没有别的调用方** —— 各个消费点（`scripts/release/package_native.py`、
`scripts/run-rayterm.sh`、Rust 常量、11 个语言包）**仍在各自内联同样的值**。

搬过去是**有意分步**的：一次性扫过所有品牌字符串，是一次没人能审的改动，
而且失败方式是静默的 —— 名字改错一处在编译和测试里都看不出来。

### 第二步（未做）

按块把内联值换成读配置，每块单独提交、单独验证：

1. `scripts/release/package_native.py` 的 `BASE_APP_NAME` / `STABLE_APP_IDENTIFIER` / 产物前缀
2. `scripts/run-rayterm.sh` 的 bundle 元数据
3. Rust 侧的品牌常量
4. 语言包里的产品名

## 校验规则里两条是真实约束，不是风格偏好

**`executableName` 必须是 `oxideterm-native`。**
它同时是**已存 AI 预设调用的 ACP 适配器命令名**。改掉它，用户已有的预设会静默失效。
所以它允许与产品名不同，且**换品牌时不得改动**。

**URL 不得带凭据、查询串或片段。**
带凭据的 URL 会随每次构建一起发布出去。查询串与片段在这里同样无用武之地：
它们是预签名链接或带跟踪链接的形态，不该出现在嵌进二进制的值里。

## 用法

```bash
cd oxideterm
python3 custom-branding/branding.py           # 打印解析后的配置
python3 custom-branding/test_branding.py      # 跑校验规则的测试
```
