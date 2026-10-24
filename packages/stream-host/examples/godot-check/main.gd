extends Node3D
## Native engine motion and input check, rendered inside the container's Xvfb.
var pawn: MeshInstance3D
var rotor: MeshInstance3D
var label: Label
var indicator: ColorRect
var input_count := 0
var elapsed := 0.0

func block(size: Vector3, color: Color, at: Vector3) -> MeshInstance3D:
	var mesh := MeshInstance3D.new()
	var shape := BoxMesh.new()
	shape.size = size
	var material := StandardMaterial3D.new()
	material.albedo_color = color
	material.roughness = 0.65
	shape.material = material
	mesh.mesh = shape
	mesh.position = at
	add_child(mesh)
	return mesh

func _ready() -> void:
	Engine.max_fps = 30
	var camera := Camera3D.new()
	add_child(camera)
	camera.position = Vector3(9, 10, 14)
	camera.look_at(Vector3(0, 0, 0))
	camera.current = true
	var sun := DirectionalLight3D.new()
	sun.rotation_degrees = Vector3(-55, -30, 0)
	sun.light_energy = 1.5
	add_child(sun)
	var environment := WorldEnvironment.new()
	environment.environment = Environment.new()
	environment.environment.ambient_light_source = Environment.AMBIENT_SOURCE_COLOR
	environment.environment.ambient_light_color = Color(0.65, 0.75, 0.9)
	environment.environment.ambient_light_energy = 0.65
	add_child(environment)
	block(Vector3(18, 0.2, 14), Color("#182538"), Vector3(0, -0.2, 0))
	for x in range(-8, 9, 2):
		block(Vector3(0.025, 0.02, 14), Color("#2d435e"), Vector3(x, -0.08, 0))
	for z in range(-6, 7, 2):
		block(Vector3(18, 0.02, 0.025), Color("#2d435e"), Vector3(0, -0.08, z))
	pawn = block(Vector3(1, 1, 1), Color("#40e3bd"), Vector3(0, 0.5, 2))
	rotor = block(Vector3(4, 0.35, 0.6), Color("#ffbc65"), Vector3(0, 1.7, -2))
	var hud := CanvasLayer.new()
	add_child(hud)
	label = Label.new()
	label.position = Vector2(28, 24)
	label.add_theme_font_size_override("font_size", 24)
	hud.add_child(label)
	var button := Button.new()
	button.text = "Test click"
	button.position = Vector2(28, 145)
	button.size = Vector2(180, 52)
	button.pressed.connect(func():
		input_count += 1
		indicator.color = Color("#e95821") if input_count % 2 == 1 else Color("#174063")
	)
	hud.add_child(button)
	indicator = ColorRect.new()
	indicator.position = Vector2(280, 200)
	indicator.size = Vector2(60, 60)
	indicator.color = Color("#174063")
	indicator.mouse_filter = Control.MOUSE_FILTER_IGNORE
	hud.add_child(indicator)

func _process(delta: float) -> void:
	elapsed += delta
	rotor.rotation.y += delta * 1.5
	rotor.position.y = 1.7 + sin(elapsed * 2.0) * 0.45
	var direction := Vector3(float(Input.is_physical_key_pressed(KEY_D)) - float(Input.is_physical_key_pressed(KEY_A)), 0,
		float(Input.is_physical_key_pressed(KEY_S)) - float(Input.is_physical_key_pressed(KEY_W)))
	pawn.position += direction.normalized() * delta * 4.0
	pawn.position.x = clampf(pawn.position.x, -8, 8)
	pawn.position.z = clampf(pawn.position.z, -6, 6)
	label.text = "Enginebox · Native Godot host check\nWASD moves the green cube · Click the button to test input\nClicks: %d   Position: %.1f, %.1f   Engine: %d fps" % [input_count, pawn.position.x, pawn.position.z, Engine.get_frames_per_second()]
