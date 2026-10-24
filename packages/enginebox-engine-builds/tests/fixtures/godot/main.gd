extends Control
var count := 0
func _ready():
    var button = Button.new()
    button.text = "Play immutable build"
    button.position = Vector2(180, 140)
    button.size = Vector2(280, 70)
    button.pressed.connect(func():
        count += 1
        button.text = "Played %d" % count
        if OS.has_feature("web"):
            JavaScriptBridge.eval("window.engineboxPlayed = %d" % count)
    )
    add_child(button)
    if OS.has_feature("web"):
        JavaScriptBridge.eval("window.engineboxReady = true")
    if "--enginebox-proof" in OS.get_cmdline_user_args():
        print("ENGINEBOX_IMMUTABLE_PLAYABLE_OK")
        get_tree().quit()
