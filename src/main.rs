use kovi::build_bot;

fn main() {
    build_bot!(kovi_plugin_cmd, msg_rank, help_msg, markdown, yu_gi_oh).run();
}
