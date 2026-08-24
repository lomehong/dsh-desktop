//! M3 预留：以非浏览器客户端身份订阅 `<base>/api/events.mux` WebSocket 下行流，
//! 解析 `server-request → session/event` 信封，把「回合完成 / 审批请求 / IM 访客消息」
//! 转成 OS 原生通知与任务栏闪烁。信封协议已实测验证：
//! {"type":"server-request","rpcId":…,"method":"session/event","payload":
//!  {"type":"session/event","sessionId":…,"event":{"type":"approval/policy","seq":…,"data":{…}}}}
