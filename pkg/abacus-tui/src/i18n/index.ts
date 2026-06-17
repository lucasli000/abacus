// i18n/index.ts — Internationalization

export type Lang = 'zh' | 'en'

const translations: Record<Lang, Record<string, string>> = {
  zh: {
    'welcome': '欢迎使用 Abacus TUI v3',
    'type_message': '输入消息并按 Enter 开始',
    'send': '发送',
    'cancel': '取消',
    'new_session': '新建会话',
    'save_session': '保存会话',
    'dashboard': '看板',
    'commands': '命令',
    'settings': '设置',
    'help': '帮助',
    'exit': '退出',
    'thinking': '思考中',
    'executing': '执行中',
    'completed': '已完成',
    'failed': '失败',
    'allow_once': '允许一次',
    'always_allow': '总是允许',
    'deny': '拒绝',
    'model': '模型',
    'theme': '主题',
    'mode': '模式',
    'tokens': 'token',
    'cost': '成本',
    'clarify': '澄清',
    'team': '团队',
    'meeting': '会议',
    'plan': '计划',
    'shortcut_send': 'Enter: 发送',
    'shortcut_commands': 'Ctrl+K: 命令',
    'shortcut_dashboard': 'Ctrl+D: 看板',
    'shortcut_exit': 'Ctrl+C: 退出',
    'auto_allow_in': '自动允许',
    'auto_deny_in': '自动拒绝',
    'seconds': '秒',
  },
  en: {
    'welcome': 'Welcome to Abacus TUI v3',
    'type_message': 'Type a message and press Enter to start',
    'send': 'Send',
    'cancel': 'Cancel',
    'new_session': 'New Session',
    'save_session': 'Save Session',
    'dashboard': 'Dashboard',
    'commands': 'Commands',
    'settings': 'Settings',
    'help': 'Help',
    'exit': 'Exit',
    'thinking': 'Thinking',
    'executing': 'Executing',
    'completed': 'Completed',
    'failed': 'Failed',
    'allow_once': 'Allow Once',
    'always_allow': 'Always Allow',
    'deny': 'Deny',
    'model': 'Model',
    'theme': 'Theme',
    'mode': 'Mode',
    'tokens': 'tokens',
    'cost': 'Cost',
    'clarify': 'Clarify',
    'team': 'Team',
    'meeting': 'Meeting',
    'plan': 'Plan',
    'shortcut_send': 'Enter: Send',
    'shortcut_commands': 'Ctrl+K: Commands',
    'shortcut_dashboard': 'Ctrl+D: Dashboard',
    'shortcut_exit': 'Ctrl+C: Exit',
    'auto_allow_in': 'Auto-allow in',
    'auto_deny_in': 'Auto-deny in',
    'seconds': 's',
  },
}

let currentLang: Lang = 'zh'

export function setLang(lang: Lang): void {
  currentLang = lang
}

export function getLang(): Lang {
  return currentLang
}

export function t(key: string, ...args: Array<string | number>): string {
  let text = translations[currentLang][key] || translations['en'][key] || key
  for (let i = 0; i < args.length; i++) {
    text = text.replace(`{${i}}`, String(args[i]))
  }
  return text
}
