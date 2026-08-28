use kovi::Bot;
use kovi::config::kovi_conf::KoviConf;
use kovi_onebot::OneBotDriver;

/// 挂载全部插件，行为与生产入口一致。
pub fn build_bot(kovi_config: KoviConf, driver: OneBotDriver) -> Bot {
    let mut bot = Bot::build(kovi_config, driver);
    let plugin_set = kovi::plugins!(
        kovi_plugin_cmd,
        msg_rank,
        help_msg,
        markdown,
        yu_gi_oh,
        bilibili,
        wordle,
        image_lib
    );
    bot.mount_plugin_set(plugin_set);
    bot.set_plugin_startup_use_file_ref();
    bot
}
