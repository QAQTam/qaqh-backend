// 验证 store.getVersion 作为独立引用调用不再丢失 this（回归测试）
import { store } from './src/state/store';

const gv = store.getVersion; // 独立引用（useSyncExternalStore 的调用方式）
const sub = store.subscribe;

const v1 = gv();
let notified = 0;
const unsub = sub(() => notified++);

store.setConn({ state: 'connecting' });
store.setConn({ state: 'open', epoch: 'e', sessionId: 's', ttlMs: 30000 });

const v2 = gv();
console.log('getVersion as bare ref:', v1, '->', v2, '(ok:', v1 === 0 && v2 === 2, ')');
console.log('notified:', notified, '(ok:', notified === 2, ')');
console.log('conn state:', store.conn.state);
unsub();
console.log('PASS');
