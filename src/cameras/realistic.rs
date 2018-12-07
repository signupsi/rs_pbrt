// std
use std;
use std::path::PathBuf;
use std::sync::Arc;
// pbrt
use core::camera::{Camera, CameraSample};
use core::film::Film;
use core::floatfile::read_float_file;
use core::geometry::{bnd2_expand, bnd2_union_pnt2, nrm_faceforward_vec3, pnt2_inside_bnd2};
use core::geometry::{Bounds2f, Normal3f, Point2f, Point3f, Ray, Vector3f};
use core::interaction::InteractionCommon;
use core::light::VisibilityTester;
use core::lowdiscrepancy::radical_inverse;
use core::medium::Medium;
use core::paramset::ParamSet;
use core::pbrt::{lerp, quadratic};
use core::pbrt::{Float, Spectrum};
use core::reflection::refract;
use core::transform::{AnimatedTransform, Transform};

// see realistic.h

#[derive(Debug, Default, Copy, Clone)]
pub struct LensElementInterface {
    pub curvature_radius: Float,
    pub thickness: Float,
    pub eta: Float,
    pub aperture_radius: Float,
}

pub struct RealisticCamera {
    // inherited from Camera (see camera.h)
    pub camera_to_world: AnimatedTransform,
    pub shutter_open: Float,
    pub shutter_close: Float,
    pub film: Arc<Film>,
    pub medium: Option<Arc<Medium + Send + Sync>>,
    // private data (see realistic.h)
    pub simple_weighting: bool,
    pub element_interfaces: Vec<LensElementInterface>,
    pub exit_pupil_bounds: Vec<Bounds2f>,
}

impl RealisticCamera {
    pub fn new(
        camera_to_world: AnimatedTransform,
        shutter_open: Float,
        shutter_close: Float,
        aperture_diameter: Float,
        focus_distance: Float,
        simple_weighting: bool,
        lens_data: &Vec<Float>,
        film: Arc<Film>,
        medium: Option<Arc<Medium + Send + Sync>>,
    ) -> Self {
        let mut element_interfaces: Vec<LensElementInterface> = Vec::new();
        for i in (0..lens_data.len()).step_by(4) {
            let mut diameter: Float = lens_data[i + 3];
            if lens_data[i] == 0.0 as Float {
                if aperture_diameter > lens_data[i + 3] {
                    println!("Specified aperture diameter {} is greater than maximum possible {}.  Clamping it.",
                             aperture_diameter,
                             lens_data[i + 3]);
                } else {
                    diameter = aperture_diameter;
                }
            }
            element_interfaces.push(LensElementInterface {
                curvature_radius: lens_data[i] * 0.001 as Float,
                thickness: lens_data[i + 1] * 0.001 as Float,
                eta: lens_data[i + 2],
                aperture_radius: diameter * 0.001 as Float / 2.0 as Float,
            });
            println!("{:?}", element_interfaces[i / 4]);
        }
        let camera = RealisticCamera {
            camera_to_world: camera_to_world,
            shutter_open: shutter_open,
            shutter_close: shutter_close,
            film: film,
            medium: medium,
            simple_weighting: simple_weighting,
            element_interfaces: element_interfaces,
            exit_pupil_bounds: Vec::new(),
        };
        // compute lens--film distance for given focus distance
        camera.focus_binary_search(focus_distance);
        // WORK
        camera
    }
    pub fn create(
        params: &ParamSet,
        cam2world: AnimatedTransform,
        film: Arc<Film>,
        medium: Option<Arc<Medium + Send + Sync>>,
        search_directory: Option<&Box<PathBuf>>,
    ) -> Arc<Camera + Send + Sync> {
        let shutteropen: Float = params.find_one_float("shutteropen", 0.0);
        let shutterclose: Float = params.find_one_float("shutterclose", 1.0);
        // TODO: std::swap(shutterclose, shutteropen);
        assert!(shutterclose >= shutteropen);
        // realistic camera-specific parameters
        let mut lens_file: String = params.find_one_filename("lensfile", String::from(""));
        if lens_file != String::from("") {
            if let Some(ref search_directory) = search_directory {
                let mut path_buf: PathBuf = PathBuf::from("/");
                path_buf.push(search_directory.as_ref());
                path_buf.push(lens_file);
                lens_file = String::from(path_buf.to_str().unwrap());
            }
        }
        if lens_file == "" {
            println!("ERROR: No lens description file supplied!");
        } else {
            println!("lens_file = {:?}", lens_file);
        }
        let aperture_diameter: Float = params.find_one_float("aperturediameter", 1.0);
        let focus_distance: Float = params.find_one_float("focusdistance", 10.0);
        let simple_weighting: bool = params.find_one_bool("simpleweighting", true);
        let mut lens_data: Vec<Float> = Vec::new();
        if !read_float_file(&lens_file, &mut lens_data) {
            println!(
                "ERROR: Error reading lens specification file {:?}.",
                lens_file
            );
        }
        if lens_data.len() % 4_usize != 0_usize {
            println!("ERROR: Excess values in lens specification file {:?}; must be multiple-of-four values, read {}.",
                     lens_file, lens_data.len());
        }
        println!("lens_data = {:?}", lens_data);
        let camera = Arc::new(RealisticCamera::new(
            cam2world,
            shutteropen,
            shutterclose,
            aperture_diameter,
            focus_distance,
            simple_weighting,
            &lens_data,
            film,
            medium,
        ));
        camera
    }
    pub fn lens_rear_z(&self) -> Float {
        self.element_interfaces.last().unwrap().thickness
    }
    pub fn lens_front_z(&self) -> Float {
        let mut z_sum = 0.0;
        for i in 0..self.element_interfaces.len() {
            let element = self.element_interfaces[i];
            z_sum += element.thickness
        }
        z_sum
    }
    pub fn rear_element_radius(&self) -> Float {
        self.element_interfaces.last().unwrap().aperture_radius
    }
    pub fn trace_lenses_from_film(&self, r_camera: &Ray, r_out: Option<&mut Ray>) -> bool {
        let mut element_z: Float = 0.0 as Float;
        // transform _rCamera_ from camera to lens system space
        let camera_to_lens: Transform = Transform::scale(1.0 as Float, 1.0 as Float, -1.0 as Float);
        let mut r_lens: Ray = camera_to_lens.transform_ray(r_camera);
        let ei_len = self.element_interfaces.len();
        for idx in 0..ei_len {
            let i = ei_len - 1 - idx;
            let element = self.element_interfaces[i];
            // update ray from film accounting for interaction with _element_
            element_z -= element.thickness;
            // compute intersection of ray with lens element
            let mut t: Float = 0.0 as Float;
            let mut n: Normal3f = Normal3f::default();
            let is_stop: bool = element.curvature_radius == 0.0 as Float;
            if is_stop {
                // The refracted ray computed in the previous lens
                // element interface may be pointed towards film
                // plane(+z) in some extreme situations; in such
                // cases, 't' becomes negative.
                if r_lens.d.z >= 0.0 as Float {
                    return false;
                }
                t = (element_z - r_lens.o.z) / r_lens.d.z;
            } else {
                let radius: Float = element.curvature_radius;
                let z_center: Float = element_z + element.curvature_radius;
                if !self.intersect_spherical_element(radius, z_center, &r_lens, &mut t, &mut n) {
                    return false;
                }
            }
            assert!(t >= 0.0 as Float);
            // test intersection point against element aperture
            let p_hit: Point3f = r_lens.position(t);
            let r2: Float = p_hit.x * p_hit.x + p_hit.y * p_hit.y;
            if r2 > element.aperture_radius * element.aperture_radius {
                return false;
            }
            r_lens.o = p_hit;
            // update ray path for element interface interaction
            if !is_stop {
                let mut w: Vector3f = Vector3f::default();
                let eta_i: Float = element.eta;
                let eta_t: Float;
                if i > 0_usize && self.element_interfaces[i - 1].eta != 0.0 as Float {
                    eta_t = self.element_interfaces[i - 1].eta;
                } else {
                    eta_t = 1.0 as Float;
                }
                if !refract(&(-r_lens.d).normalize(), &n, eta_i / eta_t, &mut w) {
                    return false;
                }
                r_lens.d = w;
            }
        }
        // transform _r_lens_ from lens system space back to camera space
        if let Some(r_out) = r_out {
            let lens_to_camera: Transform =
                Transform::scale(1.0 as Float, 1.0 as Float, -1.0 as Float);
            *r_out = lens_to_camera.transform_ray(&r_lens);
        }
        true
    }
    pub fn intersect_spherical_element(
        &self,
        radius: Float,
        z_center: Float,
        ray: &Ray,
        t: &mut Float,
        n: &mut Normal3f,
    ) -> bool {
        // compute _t0_ and _t1_ for ray--element intersection
        let o: Point3f = ray.o - Vector3f {
            x: 0.0 as Float,
            y: 0.0 as Float,
            z: z_center,
        };
        let a: Float = ray.d.x * ray.d.x + ray.d.y * ray.d.y + ray.d.z * ray.d.z;
        let b: Float = 2.0 as Float * (ray.d.x * o.x + ray.d.y * o.y + ray.d.z * o.z);
        let c: Float = o.x * o.x + o.y * o.y + o.z * o.z - radius * radius;
        let mut t0: Float = 0.0 as Float;
        let mut t1: Float = 0.0 as Float;
        if !quadratic(a, b, c, &mut t0, &mut t1) {
            return false;
        }
        // select intersection $t$ based on ray direction and element curvature
        let use_closer_t: bool = (ray.d.z > 0.0 as Float) ^ (radius < 0.0 as Float);
        if use_closer_t {
            *t = t0.min(t1);
        } else {
            *t = t0.max(t1);
        }
        if *t < 0.0 as Float {
            return false;
        }
        // compute surface normal of element at ray intersection point
        *n = Normal3f::from(Vector3f::from(o + ray.d * *t));
        *n = nrm_faceforward_vec3(&n.normalize(), &-ray.d);
        true
    }
    pub fn trace_lenses_from_scene(&self, r_camera: &Ray, r_out: Option<&mut Ray>) -> bool {
        let mut element_z: Float = -self.lens_front_z();
        // transform _r_camera_ from camera to lens system space
        let camera_to_lens: Transform = Transform::scale(1.0 as Float, 1.0 as Float, -1.0 as Float);
        let mut r_lens: Ray = camera_to_lens.transform_ray(r_camera);
        for i in 0..self.element_interfaces.len() {
            let element = self.element_interfaces[i];
            // compute intersection of ray with lens element
            let mut t: Float = 0.0 as Float;
            let mut n: Normal3f = Normal3f::default();
            let is_stop: bool = element.curvature_radius == 0.0 as Float;
            if is_stop {
                t = (element_z - r_lens.o.z) / r_lens.d.z;
            } else {
                let radius: Float = element.curvature_radius;
                let z_center: Float = element_z + element.curvature_radius;
                if !self.intersect_spherical_element(radius, z_center, &r_lens, &mut t, &mut n) {
                    return false;
                }
            }
            assert!(t >= 0.0 as Float);
            // test intersection point against element aperture
            let p_hit: Point3f = r_lens.position(t);
            let r2: Float = p_hit.x * p_hit.x + p_hit.y * p_hit.y;
            if r2 > element.aperture_radius * element.aperture_radius {
                return false;
            }
            r_lens.o = p_hit;
            // update ray path for from-scene element interface interaction
            if !is_stop {
                let mut wt: Vector3f = Vector3f::default();
                let eta_i: Float;
                if i == 0 || self.element_interfaces[i - 1].eta == 0.0 as Float {
                    eta_i = 1.0 as Float;
                } else {
                    eta_i = self.element_interfaces[i - 1].eta;
                }
                let eta_t: Float;
                if self.element_interfaces[i].eta != 0.0 as Float {
                    eta_t = self.element_interfaces[i].eta;
                } else {
                    eta_t = 1.0 as Float;
                }
                if !refract(&(-r_lens.d).normalize(), &n, eta_i / eta_t, &mut wt) {
                    return false;
                }
                r_lens.d = wt;
            }
            element_z += element.thickness;
        }
        // transform _r_lens_ from lens system space back to camera space
        if let Some(r_out) = r_out {
            let lens_to_camera: Transform =
                Transform::scale(1.0 as Float, 1.0 as Float, -1.0 as Float);
            *r_out = lens_to_camera.transform_ray(&r_lens);
        }
        true
    }
    pub fn draw_lens_system(&self) {
        // WORK
    }
    pub fn draw_ray_path_from_film(&self, r: &Ray, arrow: bool, to_optical_intercept: bool) {
        // WORK
    }
    pub fn draw_ray_path_from_scene(&self, r: &Ray, arrow: bool, to_optical_intercept: bool) {
        // WORK
    }
    pub fn compute_cardinal_points(
        &self,
        r_in: &Ray,
        r_out: &Ray,
        idx: usize,
        pz: &mut [Float; 2],
        fz: &mut [Float; 2],
    ) {
        let tf: Float = -r_out.o.x / r_out.d.x;
        fz[idx] = -r_out.position(tf).z;
        let tp: Float = (r_in.o.x - r_out.o.x) / r_out.d.x;
        pz[idx] = -r_out.position(tp).z;
    }
    pub fn compute_thick_lens_approximation(&self, pz: &mut [Float; 2], fz: &mut [Float; 2]) {
        // find height $x$ from optical axis for parallel rays
        let x: Float = 0.001 as Float * self.film.diagonal;
        // compute cardinal points for film side of lens system
        let mut r_scene: Ray = Ray {
            o: Point3f {
                x: x,
                y: 0.0 as Float,
                z: self.lens_front_z() + 1.0 as Float,
            },
            d: Vector3f {
                x: 0.0 as Float,
                y: 0.0 as Float,
                z: -1.0 as Float,
            },
            t_max: std::f32::INFINITY,
            time: 0.0 as Float,
            medium: None,
            differential: None,
        };
        let mut r_film: Ray = Ray::default();
        assert!(self.trace_lenses_from_scene(&r_scene, Some(&mut r_film)),
                "Unable to trace ray from scene to film for thick lens approximation. Is aperture stop extremely small?");
        self.compute_cardinal_points(&r_scene, &r_film, 0, pz, fz);
        // compute cardinal points for scene side of lens system
        r_film.o = Point3f {
            x: x,
            y: 0.0 as Float,
            z: self.lens_rear_z() - 1.0 as Float,
        };
        r_film.d = Vector3f {
            x: 0.0 as Float,
            y: 0.0 as Float,
            z: 1.0 as Float,
        };
        assert!(self.trace_lenses_from_film(&r_film, Some(&mut r_scene)),
                "Unable to trace ray from film to scene for thick lens approximation. Is aperture stop extremely small?");
        self.compute_cardinal_points(&r_film, &r_scene, 1, pz, fz);
    }
    pub fn focus_thick_lens(&self, focus_distance: Float) -> Float {
        let mut pz: [Float; 2] = [0.0 as Float; 2];
        let mut fz: [Float; 2] = [0.0 as Float; 2];
        self.compute_thick_lens_approximation(&mut pz, &mut fz);
        // LOG(INFO) << StringPrintf("Cardinal points: p' = %f f' = %f, p = %f f = %f.\n",
        //                           pz[0], fz[0], pz[1], fz[1]);
        // LOG(INFO) << StringPrintf("Effective focal length %f\n", fz[0] - pz[0]);
        // compute translation of lens, _delta_, to focus at _focus_distance_
        let f: Float = fz[0] - pz[0];
        let z: Float = -focus_distance;
        let c: Float = (pz[1] - z - pz[0]) * (pz[1] - z - 4.0 as Float * f - pz[0]);
        assert!(c > 0.0 as Float,
                "Coefficient must be positive. It looks focus_distance: {} is too short for a given lenses configuration",
                focus_distance);
        let delta: Float = 0.5 as Float * (pz[1] - z + pz[0] - c.sqrt());
        self.element_interfaces.last().unwrap().thickness + delta
    }
    pub fn focus_binary_search(&self, focus_distance: Float) -> Float {
        // find _film_distance_lower_, _film_distance_upper_ that bound focus distance
        let mut film_distance_upper: Float = self.focus_thick_lens(focus_distance);
        let mut film_distance_lower: Float = film_distance_upper;
        while self.focus_distance(film_distance_lower) > focus_distance {
            film_distance_lower *= 1.005 as Float;
        }
        while self.focus_distance(film_distance_upper) < focus_distance {
            film_distance_upper /= 1.005 as Float;
        }
        // do binary search on film distances to focus
        // for (int i = 0; i < 20; ++i) {
        //     Float fmid = 0.5f * (film_distance_lower + film_distance_upper);
        //     Float midFocus = self.focus_distance(fmid);
        //     if (midFocus < focus_distance)
        //         film_distance_lower = fmid;
        //     else
        //         film_distance_upper = fmid;
        // }
        // return 0.5f * (film_distance_lower + film_distance_upper);
        // WORK
        0.0
    }
    pub fn focus_distance(&self, film_dist: Float) -> Float {
        // find offset ray from film center through lens
        let bounds: Bounds2f =
            self.bound_exit_pupil(0.0 as Float, 0.001 as Float * self.film.diagonal);
        // const std::array<Float, 3> scaleFactors = {0.1f, 0.01f, 0.001f};
        // Float lu = 0.0f;

        // Ray ray;

        // // Try some different and decreasing scaling factor to find focus ray
        // // more quickly when `aperturediameter` is too small.
        // // (e.g. 2 [mm] for `aperturediameter` with wide.22mm.dat),
        // bool foundFocusRay = false;
        // for (Float scale : scaleFactors) {
        //     lu = scale * bounds.pMax[0];
        //     if (TraceLensesFromFilm(Ray(Point3f(0, 0, LensRearZ() - filmDistance),
        //                                 Vector3f(lu, 0, filmDistance)),
        //                             &ray)) {
        //         foundFocusRay = true;
        //         break;
        //     }
        // }

        // if (!foundFocusRay) {
        //     Error(
        //         "Focus ray at lens pos(%f,0) didn't make it through the lenses "
        //         "with film distance %f?!??\n",
        //         lu, filmDistance);
        //     return Infinity;
        // }

        // // Compute distance _zFocus_ where ray intersects the principal axis
        // Float tFocus = -ray.o.x / ray.d.x;
        // Float zFocus = ray(tFocus).z;
        // if (zFocus < 0) zFocus = Infinity;
        // return zFocus;
        // WORK
        0.0
    }
    pub fn bound_exit_pupil(&self, p_film_x0: Float, p_film_x1: Float) -> Bounds2f {
        let mut pupil_bounds: Bounds2f = Bounds2f::default();
        // sample a collection of points on the rear lens to find exit pupil
        let n_samples: i32 = 1024 * 1024;
        let mut n_exiting_rays: i32 = 0;
        // compute bounding box of projection of rear element on sampling plane
        let rear_radius: Float = self.rear_element_radius();
        let proj_rear_bounds: Bounds2f = Bounds2f {
            p_min: Point2f {
                x: -1.5 as Float * rear_radius,
                y: -1.5 as Float * rear_radius,
            },
            p_max: Point2f {
                x: 1.5 as Float * rear_radius,
                y: 1.5 as Float * rear_radius,
            },
        };
        for i in 0..n_samples {
            // find location of sample points on $x$ segment and rear lens element
            let p_film: Point3f = Point3f {
                x: lerp(
                    (i as Float + 0.5 as Float) / n_samples as Float,
                    p_film_x0,
                    p_film_x1,
                ),
                y: 0.0 as Float,
                z: 0.0 as Float,
            };
            let u: [Float; 2] = [
                radical_inverse(0 as u16, i as u64),
                radical_inverse(1 as u16, i as u64),
            ];
            let p_rear: Point3f = Point3f {
                x: lerp(u[0], proj_rear_bounds.p_min.x, proj_rear_bounds.p_max.x),
                y: lerp(u[1], proj_rear_bounds.p_min.y, proj_rear_bounds.p_max.y),
                z: self.lens_rear_z(),
            };
            // expand pupil bounds if ray makes it through the lens system
            if pnt2_inside_bnd2(
                &Point2f {
                    x: p_rear.x,
                    y: p_rear.y,
                },
                &pupil_bounds,
            ) || self.trace_lenses_from_film(
                &Ray {
                    o: p_film,
                    d: p_rear - p_film,
                    t_max: std::f32::INFINITY,
                    time: 0.0 as Float,
                    medium: None,
                    differential: None,
                },
                None,
            ) {
                pupil_bounds = bnd2_union_pnt2(
                    &pupil_bounds,
                    &Point2f {
                        x: p_rear.x,
                        y: p_rear.y,
                    },
                );
                n_exiting_rays += 1;
            }
        }
        // return entire element bounds if no rays made it through the lens system
        if n_exiting_rays == 0_i32 {
            println!(
                "Unable to find exit pupil in x = [{},{}] on film.",
                p_film_x0, p_film_x1
            );
            return proj_rear_bounds;
        }
        // expand bounds to account for sample spacing
        pupil_bounds = bnd2_expand(
            &pupil_bounds,
            2.0 as Float * proj_rear_bounds.diagonal().length() / (n_samples as Float).sqrt(),
        );
        pupil_bounds
    }
    pub fn render_exit_pupil(&self, sx: Float, sy: Float, filename: String) {
        // WORK
    }
    pub fn sample_exit_pupil(
        &self,
        p_film: &Point2f,
        lens_sample: &Point2f,
        sample_bounds_area: &mut Float,
    ) -> Point3f {
        // WORK
        Point3f::default()
    }
    pub fn test_exit_pupil_bounds(&self) {
        // WORK
    }
}

impl Camera for RealisticCamera {
    fn generate_ray_differential(&self, sample: &CameraSample, ray: &mut Ray) -> Float {
        // WORK
        0.0
    }
    fn we(&self, _ray: &Ray, _p_raster2: Option<&mut Point2f>) -> Spectrum {
        panic!("camera::we() is not implemented!");
        // Spectrum::default()
    }
    fn pdf_we(&self, _ray: &Ray) -> (Float, Float) {
        // let mut pdf_pos: Float = 0.0;
        // let mut pdf_dir: Float = 0.0;
        panic!("camera::pdf_we() is not implemented!");
        // (pdf_pos, pdf_dir)
    }
    fn sample_wi(
        &self,
        _iref: &InteractionCommon,
        _u: &Point2f,
        _wi: &mut Vector3f,
        _pdf: &mut Float,
        _p_raster: &mut Point2f,
        _vis: &mut VisibilityTester,
    ) -> Spectrum {
        panic!("camera::sample_wi() is not implemented!");
        // Spectrum::default()
    }
    fn get_film(&self) -> Arc<Film> {
        self.film.clone()
    }
}
