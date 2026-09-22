// ARC「交 PR / 合并」的前置检查。
// exit 0 = 有事可做；exit 2 = 无事，跳过（零 token）；其它 = 意外，fail-closed。
//
// 两条实测教训，都是装上去之后当场暂出来的：
//
// 1. **必须快。** 第一版把 `run_lite.py --check` 放进来，而它要翻十页运行列表
//    再拉每个在跑运行的日志，自测 **30s 超时 → block**，那会把每一轮都
//    fail-closed 掉，比不装钩子还糟。lite 由专职任务 47134b1b 负责，这里不重复查，
//    顺便避开两个任务同时 `--go` 双重起运行的竞争。
//
// 2. **只算「我们能动手的」PR。** 第二版数所有开着的 PR，于是被别人的 #207
//    （amosgeek，2564 文件、−901K 行、且 CONFLICTING）永久卡住：每 2 小时唤醒
//    一个烧 token 的 agent 轮次，去看一个它本就不该合的 PR。
//    所以只数**我们自己的、且没有冲突的** PR。
//    别人的大 PR 是人的决定，不属于定时任务的职责范围。
//
// 有事可做（任一）：未提交改动 / 领先 origin/main / 我们自己可合的 PR / web 监督者死了
import { execSync } from 'node:child_process';

const REPO = '/Users/mac/Desktop/code/octos-org/octos-arc-0917';
const OURS = 'BH3GEI';
const reasons = [];

function probe(cmd, timeout) {
  try {
    const out = execSync(cmd, { cwd: REPO, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout });
    return { code: 0, out: String(out).trim() };
  } catch (e) {
    return { code: typeof e.status === 'number' ? e.status : -1, out: String(e.stdout || '').trim() };
  }
}

function must(cmd, timeout) {
  return String(execSync(cmd, { cwd: REPO, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout })).trim();
}

try {
  const dirty = must('git status --porcelain', 15000);
  if (dirty) reasons.push(`未提交改动 ${dirty.split('\n').length} 处`);

  // fetch 允许失败（离线）；失败就拿本地已知的 origin/main 比
  probe('git fetch origin --quiet', 12000);
  const ahead = must('git log --oneline origin/main..HEAD', 15000);
  if (ahead) reasons.push(`领先 main ${ahead.split('\n').length} 个提交`);

  // 只数我们自己的、非 CONFLICTING 的 PR。gh 未登录/限流是「查不到」而不是「没有」，
  // 所以只在它确实给出数字时才算。mergeable 刚建时可能是 UNKNOWN，
  // 那也算我们该看的（只排除已知冲突的）。
  const jq = `[.[] | select(.author.login == "${OURS}" and .mergeable != "CONFLICTING")] | length`;
  const pr = probe(`gh pr list --state open --json number,author,mergeable --jq '${jq}'`, 15000);
  if (pr.code === 0 && pr.out && pr.out !== '0') reasons.push(`${pr.out} 个我们的 PR 待合`);

  const sup = probe('pgrep -f run_web.py', 8000);
  if (sup.code !== 0 || !sup.out) reasons.push('web 监督者不在了');

  if (reasons.length) {
    console.log('放行：' + reasons.join('；'));
    process.exit(0);
  }
  console.log('无事可做：工作树干净、无领先提交、我们无待合 PR、监督者在跑');
  process.exit(2);
} catch (err) {
  console.error('前置检查本身出错，fail-closed 阻止本轮：' + (err && err.message ? err.message : String(err)));
  process.exit(1);
}
