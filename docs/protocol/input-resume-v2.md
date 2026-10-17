# terminal.input_resume v2 (SESS-04)

2026-10-13。跨仓决策与Apple安全存储见 `astra-apple/docs/adr/0007-persistent-input-resume.md`。
依赖 `terminal.input_lease` v1（间接依赖session.objects），capability未协商时不发送新字段。

AttachRequest新增tag6 `client_instance_id`：canonical UUID，36字节。空值表示本客户端未启用持久恢复（例如Keychain不可用）；非空且能力未选择返回capability错误。resume_token长度上限128bytes，新token为服务端随机UUID，不反射任意用户token。设备UUID本身不授权；必须同时有当前terminal恢复记录中的token。

AttachResponse新增tag9 `recovery_generation`：v2持久controller必为非零UInt64，viewer/非持久controller为0。每次新grant/takeover在同一lease锁下checked递增，resume保持代次，Terminal UUID不重用；不能用请求发送顺序代替该代次。计数溢出拒绝grant，不回绕。未发布的v1候选不再协商恢复，v1/v2无交集时走COMPAT-06。

同一个Terminal的lease锁线性化grant/resume/takeover/RELEASE。恢复记录最多1份，含设备UUID、token（控制权代际）、7天单调时钟期限，存于该认证用户的SessionManager Terminal内。成功resume/renew延长期限，resume保留token但旋转lease ID；普通EOF/detach与15秒lease TTL仅清短期lease，主动RELEASE同时清恢复记录。任何新grant/takeover（包括N-1）替换/撤销前一代资格。新controller后来断开、过期或释放，也不会复活之前的token。worker退出时PTY和恢复记录一并销毁，无跨worker迁移承诺。

无效资格统一返回lease_conflict，不能因无活动lease就新grant旧token。客户端一次回退viewer，不强制takeover。显式新grant/takeover属于不同请求，不由恢复重试自动产生。并发同设备resume允许最后线性化lease获胜；旧input/resize/renew/release/cleanup均通过原有lease+attachment+sequence fencing失败或无效，不影响新owner。

兼容COMPAT-06：无新能力时维持stream lease与已协商的TTL；旧token仅能匹配仍活动的旧lease，不能恢复已经清除的lease。旧客户端正常新建、viewer与显式takeover保持；重连可能需要手动接管，这是失效资格的保守行为。删除条件为新能力必选且N/N-1窗口结束，最早application v4。

managed gateway会把协商结果发给worker再次验证；部署新gateway但保留不支持新能力的旧worker会拒绝该能力，不代表恢复已经上线。WORK-03尚未完成，生产全worker升级须维护窗口，不能自动销毁既有PTY。

验证：terminal.rs覆盖EOF/TTL、错误设备、释放、到期、N-1 takeover、旧input/resize/renew/cleanup及32轮并发resume/takeover；Apple真实Quinn rootless/managed覆盖客户端对象与持久store重建、超过TTL、真实PTY输入、viewer、他端接管后断开和RELEASE。Mac/iPhone物理生命周期验收独立记录，不由上述测试代替。

客户端Keychain采用带revision的跨进程CAS；保留服务端代次及撤销墓碑，拒绝迟到旧grant覆盖新token。客户端不以本地首次保存时间判断七天资格，不把发出的RENEW当作ACK；服务端续租处理与单调时钟期限是唯一权威。具体容量/失败及v1候选隔离见ADR0007。


2026-10-13 R4/R5补正不改wire：同设备resume保留token，只有匹配该lease ID的LeaseChanged(released)才证明RELEASE被服务端接受；旧lease发送RELEASE不允许客户端提前删除共享token。Apple本地v3 vault用请求前取得的incarnation Operation隔离缓存回收；成功CloseTerminal ACK删除记录，512槽LRU回收不会让旧响应重新建项。服务端仍是唯一恢复/租约权威，具体并发、超时与旧候选隔离见ADR0007。
