blueprints = custom_target('blueprints',
  input: files(
    'blueprints/help-overlay.blp',
    'blueprints/window.blp',
  ),
  output: '.',
  command: [find_program('blueprint-compiler'), 'batch-compile', '@OUTPUT@', '@CURRENT_SOURCE_DIR@', '@INPUT@'],
)

scalable_dir = join_paths('icons', 'hicolor', 'scalable', 'apps')
install_data(
  join_paths(scalable_dir, ('@0@.svg').format(application_id)),
  install_dir: join_paths(get_option('datadir'), scalable_dir)
)
install_data(
  join_paths(scalable_dir, ('@0@.svg').format(base_id)),
  install_dir: join_paths(get_option('datadir'), scalable_dir)
)

message(join_paths(pkgdatadir, 'icons', scalable_dir))

symbolic_dir = join_paths('icons', 'hicolor', 'symbolic', 'apps')
install_data(
  join_paths(symbolic_dir, ('@0@-symbolic.svg').format('io.gitlab.adhami3310.Impression')),
  install_dir: join_paths(get_option('datadir'), symbolic_dir)
)

install_data(
  action_icons,
  install_dir: join_paths(get_option('datadir'), action_dir)
)


# Resources
resources = gnome.compile_resources(
    'resources',
    'resources.gresource.xml',
    gresource_bundle: true,
    source_dir: meson.current_build_dir(),
    install: true,
    install_dir: pkgdatadir,
    dependencies: blueprints,
)