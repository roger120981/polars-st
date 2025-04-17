use std::{cell::RefCell, collections::HashMap};

use crate::{
    args::{
        BufferKwargs, ConcaveHullKwargs, DelaunayTrianlesKwargs, OffsetCurveKwargs,
        SetPrecisionKwargs, SpatialJoinPredicate, ToGeoJsonKwargs, ToWkbKwargs, ToWktKwargs,
        VoronoiKwargs,
    },
    arity::{
        broadcast_try_binary_elementwise_values, broadcast_try_ternary_elementwise_values,
        try_ternary_elementwise_values, try_unary_elementwise_values_with_dtype,
    },
    wkb::{read_ewkb_header, WKBGeometryType},
};
use geos::{
    BufferParams, CoordSeq, GResult, GeoJSONWriter, Geom, Geometry,
    GeometryTypes::{self, *},
    PreparedGeometry, STRtree, SpatialIndex, WKBWriter, WKTWriter,
};

use polars::prelude::arity::{broadcast_try_binary_elementwise, try_unary_elementwise};
use polars::prelude::*;
use polars_arrow::array::{Array, BinaryViewArray};
use proj4rs::errors::Error as ProjError;
use proj4rs::Proj;
use pyo3::prelude::*;
use pyo3_polars::export::polars_core::utils::arrow::array::Float64Array;

pub trait GeometryUtils {
    fn to_ewkb(&self) -> GResult<Vec<u8>>;

    fn cast(&self, into: GeometryTypes) -> GResult<Geometry>;

    #[rustfmt::skip]
    #[allow(clippy::too_many_arguments)]
    fn apply_affine_transform(
        &self,
        m11: f64, m12: f64, m13: f64,
        m21: f64, m22: f64, m23: f64,
        m31: f64, m32: f64, m33: f64,
        tx:  f64, ty:  f64, tz:  f64,
    ) -> GResult<Geometry>;

    fn translate(&self, x: f64, y: f64, z: f64) -> GResult<Geometry>;
    fn rotate(&self, angle: f64, x0: f64, y0: f64) -> GResult<Geometry>;
    fn scale(&self, x: f64, y: f64, z: f64, x0: f64, y0: f64, z0: f64) -> GResult<Geometry>;
    fn skew(&self, x: f64, y: f64, z: f64, x0: f64, y0: f64, z0: f64) -> GResult<Geometry>;
}

impl<T> GeometryUtils for T
where
    T: Geom,
{
    fn to_ewkb(&self) -> GResult<Vec<u8>> {
        let mut writer = WKBWriter::new()?;
        writer.set_include_SRID(true);
        Ok(writer.write_wkb(self)?.into())
    }

    #[allow(clippy::too_many_lines)]
    fn cast(&self, into: GeometryTypes) -> GResult<Geometry> {
        let srid = self.get_srid()?;
        let mut result = match (self.geometry_type(), into) {
            (from, to) if from == to => Ok(Geom::clone(self)),
            (t, GeometryCollection) => {
                if t.is_collection() {
                    let geoms = (0..self.get_num_geometries()?)
                        .map(|n| Ok(self.get_geometry_n(n)?.clone()))
                        .collect::<Result<_, _>>()?;
                    Geometry::create_geometry_collection(geoms)
                } else {
                    Geometry::create_geometry_collection(vec![Geom::clone(self.original())])
                }
            }
            (Point, MultiPoint) => {
                if self.is_empty()? {
                    Geometry::create_multipoint(vec![])
                } else {
                    Geometry::create_multipoint(vec![Geom::clone(self)])
                }
            }
            (LineString | CircularString, MultiPoint) => {
                let coords = self.get_coord_seq()?;
                let has_z = self.has_z()?;
                let has_m = self.has_m()?;
                let dimensions = 2 + usize::from(has_z) + usize::from(has_m);
                let buffer = coords.as_buffer(Some(dimensions))?;
                buffer
                    .chunks_exact(dimensions)
                    .map(|coord| {
                        let seq = CoordSeq::new_from_buffer(coord, 1, has_z, has_m)?;
                        Geometry::create_point(seq)
                    })
                    .try_collect()
                    .and_then(Geometry::create_multipoint)
            }
            (MultiPoint, LineString | CircularString) => {
                let has_z = self.has_z()?;
                let has_m = self.has_m()?;
                let collection_size = self.get_num_geometries()?;
                let dimensions = 2 + usize::from(has_z) + usize::from(has_m);
                let mut coords = Vec::with_capacity(dimensions * collection_size);
                for n in 0..collection_size {
                    let point = self.get_geometry_n(n)?;
                    if !point.is_empty()? {
                        let mut seq = point.get_coord_seq()?.as_buffer(Some(dimensions))?;
                        coords.append(&mut seq);
                    }
                }
                let coords_size = coords.len() / dimensions;
                let coords = CoordSeq::new_from_buffer(&coords, coords_size, has_z, has_m)?;
                match into {
                    LineString => Geometry::create_line_string(coords),
                    CircularString => Geometry::create_circular_string(coords),
                    _ => unreachable!(),
                }
            }
            (CircularString, LineString) => Geometry::create_line_string(self.get_coord_seq()?),
            (LineString, CircularString) => Geometry::create_circular_string(self.get_coord_seq()?),
            (LineString | CircularString, MultiLineString) if self.is_empty()? => {
                Geometry::create_multiline_string(vec![])
            }
            (LineString, MultiLineString) => {
                Geometry::create_multiline_string(vec![Geom::clone(self)])
            }
            (CircularString, MultiLineString) => {
                let as_line = Geometry::create_circular_string(self.get_coord_seq()?)?;
                Geometry::create_multiline_string(vec![as_line])
            }
            (LineString | CircularString, MultiCurve) => {
                if self.is_empty()? {
                    Geometry::create_multicurve(vec![])
                } else {
                    Geometry::create_multicurve(vec![Geom::clone(self)])
                }
            }
            (MultiLineString, Polygon) => {
                let mut rings = (0..self.get_num_geometries()?).map(|n| {
                    Geometry::create_linear_ring(self.get_geometry_n(n)?.get_coord_seq()?)
                });
                match self.get_num_geometries()? {
                    0 => Geometry::create_empty_polygon(),
                    1 => Geometry::create_polygon(rings.next().unwrap()?, vec![]),
                    _ => {
                        let outer = rings.next().unwrap()?;
                        let inner = rings.collect::<Result<_, _>>()?;
                        Geometry::create_polygon(outer, inner)
                    }
                }
            }
            (MultiPolygon, MultiSurface) => {
                let geoms = (0..self.get_num_geometries()?)
                    .map(|n| Ok(self.get_geometry_n(n)?.clone()))
                    .collect::<Result<_, _>>()?;
                Geometry::create_multisurface(geoms)
            }
            (Polygon, MultiPolygon) => {
                if self.is_empty()? {
                    Geometry::create_multipolygon(vec![])
                } else {
                    Geometry::create_multipolygon(vec![Geom::clone(self)])
                }
            }
            (Polygon | CurvePolygon, MultiSurface) => {
                if self.is_empty()? {
                    Geometry::create_multisurface(vec![])
                } else {
                    Geometry::create_multisurface(vec![Geom::clone(self)])
                }
            }
            (from, to) => Err(geos::Error::GenericError(format!(
                "invalid cast from {from:?} to {to:?}"
            ))),
        }?;
        result.set_srid(srid);
        Ok(result)
    }

    #[rustfmt::skip]
    fn apply_affine_transform(
        &self,
        m11: f64, m12: f64, m13: f64,
        m21: f64, m22: f64, m23: f64,
        m31: f64, m32: f64, m33: f64,
        tx:  f64, ty:  f64, tz:  f64,
    ) -> GResult<Geometry> {
        let dims: u32 = self.get_coordinate_dimension()?.into();
        if dims < 3 {
            self.transform_xy(|x, y| {
                let new_x = x * m11 + y * m12 + tx;
                let new_y = x * m21 + y * m22 + ty;
                Some((new_x, new_y))
            })
        } else {
            self.transform_xyz(|x, y, z| {
                let new_x = x * m11 + y * m12 + m13 * z + tx;
                let new_y = x * m21 + y * m22 + m23 * z + ty;
                let new_z = x * m31 + y * m32 + m33 * z + tz;
                Some((new_x, new_y, new_z))
            })
        }
    }

    #[rustfmt::skip]
    fn translate(&self, x: f64, y: f64, z: f64) -> GResult<Geometry> {
        self.apply_affine_transform(
            1., 0., 0.,
            0., 1., 0.,
            0., 0., 1.,
            x,  y,  z,
        )
    }

    #[rustfmt::skip]
    fn rotate(&self, angle: f64, x0: f64, y0: f64) -> GResult<Geometry> {
        let angle = angle.to_radians();
        let cosp = angle.cos();
        let sinp = angle.sin();
        self.apply_affine_transform(
            cosp,-sinp, 0.0,
            sinp, cosp, 0.0,
            0.0,   0.0, 1.0,
            x0 - x0 * cosp + y0 * sinp,
            y0 - x0 * sinp - y0 * cosp,
            0.0,
        )
    }

    #[rustfmt::skip]
    fn scale(&self, x: f64, y: f64, z: f64, x0: f64, y0: f64, z0: f64) -> GResult<Geometry> {
        self.apply_affine_transform(
            x,  0., 0.,
            0., y,  0.,
            0., 0., z,
            x0 - x0 * x,
            y0 - y0 * y,
            z0 - z0 * z,
        )
    }

    #[rustfmt::skip]
    fn skew(&self, x: f64, y: f64, z: f64, x0: f64, y0: f64, z0: f64) -> GResult<Geometry> {
        let x = x.to_radians().tan();
        let y = y.to_radians().tan();
        let z = z.to_radians().tan();

        self.apply_affine_transform(
            1., x,  y,
            z,  1., x,
            y,  z, 1.,
            x0 - x0 * 1. - y0 * x - z0 * y,
            y0 - x0 * z - y0 * 1. - z0 * x,
            z0 - x0 * y - y0 * z - z0 * 1.,
        )
    }
}

pub fn from_wkb(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.to_ewkb())
}

pub fn from_wkt(wkt: &StringChunked) -> GResult<BinaryChunked> {
    wkt.try_apply_nonnull_values_generic(|wkt| Geometry::new_from_wkt(wkt)?.to_ewkb())
}

pub fn from_geojson(json: &StringChunked) -> GResult<BinaryChunked> {
    json.try_apply_nonnull_values_generic(|json| Geometry::new_from_geojson(json)?.to_ewkb())
}

fn get_coordinate_seq_from_array(a: Box<dyn Array>) -> GResult<CoordSeq> {
    let coords = unsafe { a.as_any().downcast_ref_unchecked::<LargeListArray>() };
    if coords.len() - coords.null_count() == 0 {
        return CoordSeq::new(0, geos::CoordDimensions::TwoD);
    }
    let offsets = coords.offsets();
    let lengths: Vec<usize> = offsets.lengths().collect();
    let is_uniform = coords.len() == 1 || lengths.windows(2).all(|s| s[0] == s[1]);
    if !is_uniform {
        let msg = "invalid coordinates list: must be uniform".into();
        return Err(geos::Error::GenericError(msg));
    }
    let dimension = lengths[0];
    let has_z = dimension > 2;
    let has_m = dimension > 3;
    let start = (*offsets.first()).try_into().unwrap();
    let values = &coords
        .values()
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap()
        .as_slice()
        .unwrap()[start..(start + coords.len() * dimension)];
    CoordSeq::new_from_buffer(values, values.len() / dimension, has_z, has_m)
}

pub fn point(coords: &ListChunked) -> GResult<BinaryChunked> {
    coords.try_apply_nonnull_values_generic(|coord| {
        let coord = unsafe { coord.as_any().downcast_ref_unchecked::<Float64Array>() };
        let dimension = coord.len();
        let has_z = dimension > 2;
        let has_m = dimension > 3;
        let coord = coord.as_slice().unwrap();
        let coord_seq = CoordSeq::new_from_buffer(coord, 1, has_z, has_m)?;
        Geometry::create_point(coord_seq)?.to_ewkb()
    })
}

pub fn multipoint(coords: &ListChunked) -> GResult<BinaryChunked> {
    coords.try_apply_nonnull_values_generic(|coords| {
        // TODO: Avoid intermediate LineString representation
        let coord_seq = get_coordinate_seq_from_array(coords)?;
        Geometry::create_line_string(coord_seq)?
            .cast(MultiPoint)?
            .to_ewkb()
    })
}

pub fn linestring(coords: &ListChunked) -> GResult<BinaryChunked> {
    coords.try_apply_nonnull_values_generic(|coords| {
        let coord_seq = get_coordinate_seq_from_array(coords)?;
        Geometry::create_line_string(coord_seq)?.to_ewkb()
    })
}

pub fn circularstring(coords: &ListChunked) -> GResult<BinaryChunked> {
    coords.try_apply_nonnull_values_generic(|coords| {
        let coord_seq = get_coordinate_seq_from_array(coords)?;
        Geometry::create_circular_string(coord_seq)?.to_ewkb()
    })
}

pub fn multilinestring(coords: &ListChunked) -> GResult<BinaryChunked> {
    fn get_line_n(coords: &LargeListArray, n: usize) -> GResult<Geometry> {
        Geometry::create_line_string(match unsafe { coords.get_unchecked(n) } {
            Some(array) => get_coordinate_seq_from_array(array),
            None => CoordSeq::new(0, geos::CoordDimensions::TwoD),
        }?)
    }

    coords.try_apply_nonnull_values_generic(|a| {
        let lines = unsafe { a.as_any().downcast_ref_unchecked::<LargeListArray>() };
        let lines = (0..lines.len())
            .map(|n| get_line_n(lines, n))
            .try_collect()?;
        Geometry::create_multiline_string(lines)?.to_ewkb()
    })
}

pub fn polygon(coords: &ListChunked) -> GResult<BinaryChunked> {
    fn get_ring_n(coords: &LargeListArray, n: usize) -> GResult<Geometry> {
        Geometry::create_linear_ring(match unsafe { coords.get_unchecked(n) } {
            Some(array) => get_coordinate_seq_from_array(array),
            None => CoordSeq::new(0, geos::CoordDimensions::TwoD),
        }?)
    }

    coords.try_apply_nonnull_values_generic(|a| {
        let rings = unsafe { a.as_any().downcast_ref_unchecked::<LargeListArray>() };
        if rings.len() == 0 {
            return Geometry::create_empty_polygon()?.to_ewkb();
        }
        let exterior = get_ring_n(rings, 0)?;
        let interiors = (1..rings.len())
            .map(|n| get_ring_n(rings, n))
            .try_collect()?;
        Geometry::create_polygon(exterior, interiors)?.to_ewkb()
    })
}

pub fn get_type_id(wkb: &BinaryChunked) -> GResult<UInt32Chunked> {
    wkb.try_apply_nonnull_values_generic(|mut wkb| {
        read_ewkb_header(&mut wkb)
            .map_err(|_| geos::Error::GenericError("Invalid WKB header".into()))
            .map(|header| WKBGeometryType::try_from(header.base_type))?
            .map_err(|e| geos::Error::GenericError(format!("Invalid geometry type: {e}")))
            .map(u32::from)
    })
}

pub fn get_num_dimensions(wkb: &BinaryChunked) -> GResult<Int32Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.geometry_type() == GeometryCollection && geom.is_empty()? {
            Ok(-1)
        } else {
            Ok(geom.get_num_dimensions()? as i32)
        }
    })
}

pub fn get_coordinate_dimension(wkb: &BinaryChunked) -> GResult<UInt32Chunked> {
    wkb.try_apply_nonnull_values_generic(|mut wkb| {
        read_ewkb_header(&mut wkb)
            .map_err(|_| geos::Error::GenericError("Invalid WKB header".into()))
            .map(|header| 2 + u32::from(header.has_z) + u32::from(header.has_m))
    })
}

pub fn get_srid(wkb: &BinaryChunked) -> GResult<Int32Chunked> {
    wkb.try_apply_nonnull_values_generic(|mut wkb| {
        read_ewkb_header(&mut wkb)
            .map_err(|_| geos::Error::GenericError("Invalid WKB header".into()))
            .map(|header| header.srid)
    })
}

pub fn set_srid(wkb: &BinaryChunked, srid: &Int32Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, srid, |wkb, srid| {
        let mut geom = Geometry::new_from_wkb(wkb)?;
        geom.set_srid(srid);
        geom.to_ewkb()
    })
}

pub fn get_x(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.geometry_type() == Point && !geom.is_empty()? {
            geom.get_x()
        } else {
            Ok(f64::NAN)
        }
    })
}

pub fn get_y(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.geometry_type() == Point && !geom.is_empty()? {
            geom.get_y()
        } else {
            Ok(f64::NAN)
        }
    })
}

pub fn get_z(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.geometry_type() == Point && !geom.is_empty()? {
            geom.get_z()
        } else {
            Ok(f64::NAN)
        }
    })
}

pub fn get_m(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.geometry_type() == Point && !geom.is_empty()? {
            geom.get_m()
        } else {
            Ok(f64::NAN)
        }
    })
}

pub fn get_exterior_ring(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    try_unary_elementwise(wkb, |wkb| {
        if let Some(wkb) = wkb {
            let geom = Geometry::new_from_wkb(wkb)?;
            if geom.geometry_type() == Polygon {
                return Ok(Some(geom.get_exterior_ring()?.to_ewkb()?));
            }
        }
        Ok(None)
    })
}

pub fn get_interior_rings(wkb: &BinaryChunked) -> GResult<ListChunked> {
    // TODO: use try_apply_nonnull_values_generic once pola-rs/polars#22233 is merged
    let dt = DataType::List(Box::new(DataType::Binary));
    let adt = dt.to_arrow(CompatLevel::newest());
    try_unary_elementwise_values_with_dtype(wkb, dt, |wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.geometry_type() != Polygon {
            let rings = BinaryViewArray::new_empty(adt.clone());
            return Ok(Box::new(rings) as Box<dyn Array>);
        }
        let num_rings = geom.get_num_interior_rings()?;
        let rings = BinaryViewArray::try_arr_from_iter((0..num_rings).map(|n| {
            let ring = geom.get_interior_ring_n(n)?;
            ring.to_ewkb()
        }))?;
        Ok(Box::new(rings) as Box<dyn Array>)
    })
}

pub fn get_num_points(wkb: &BinaryChunked) -> GResult<UInt32Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.geometry_type() {
            LineString | LinearRing => Ok(geom.get_num_points()? as u32),
            _ => Ok(0),
        }
    })
}

pub fn get_num_interior_rings(wkb: &BinaryChunked) -> GResult<UInt32Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.geometry_type() {
            Polygon => Ok(geom.get_num_interior_rings()? as u32),
            _ => Ok(0),
        }
    })
}

pub fn get_num_geometries(wkb: &BinaryChunked) -> GResult<UInt32Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .get_num_geometries()
            .map(|n| n as u32)
    })
}

pub fn get_num_coordinates(wkb: &BinaryChunked) -> GResult<UInt32Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .get_num_coordinates()
            .map(|n| n as u32)
    })
}

pub fn get_coordinates(
    wkb_array: &BinaryChunked,
    dimension: Option<usize>,
) -> GResult<ListChunked> {
    fn get_coords_sequence<T>(
        geom: &T,
        dimension: usize,
        builder: &mut ListPrimitiveChunkedBuilder<Float64Type>,
    ) -> GResult<()>
    where
        T: Geom,
    {
        match geom.geometry_type() {
            _ if geom.is_empty()? => Ok(()),
            Point | LineString | LinearRing | CircularString => {
                let coord_seq = geom.get_coord_seq()?.as_buffer(Some(dimension))?;
                for coord in coord_seq.chunks_exact(dimension) {
                    builder.append_slice(coord);
                }
                Ok(())
            }
            Polygon | CurvePolygon => {
                let coord_seq = geom
                    .get_exterior_ring()?
                    .get_coord_seq()?
                    .as_buffer(Some(dimension))?;
                for coord in coord_seq.chunks_exact(dimension) {
                    builder.append_slice(coord);
                }
                (0..geom.get_num_interior_rings()?).try_for_each(|n| {
                    get_coords_sequence(&geom.get_interior_ring_n(n)?, dimension, builder)
                })
            }
            MultiPoint | MultiLineString | MultiCurve | CompoundCurve | MultiPolygon
            | MultiSurface | GeometryCollection => {
                (0..geom.get_num_geometries()?).try_for_each(|n| {
                    get_coords_sequence(&geom.get_geometry_n(n)?, dimension, builder)
                })
            }
        }
    }
    fn get_coordinates(wkb: &[u8], dimension: Option<usize>) -> GResult<Series> {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return Ok(Series::new_null("".into(), 0));
        }
        let geom_dimension: u32 = geom.get_coordinate_dimension()?.into();
        let geom_dimension = geom_dimension as usize;
        let output_dimension = dimension.unwrap_or(geom_dimension);
        let component_count = wkb.len() / 8; // rough estimate
        let coordinates_count = component_count / geom_dimension;
        let mut builder = ListPrimitiveChunkedBuilder::<Float64Type>::new(
            "".into(),
            coordinates_count,
            coordinates_count * output_dimension,
            DataType::Float64,
        );
        get_coords_sequence(&geom, output_dimension, &mut builder)?;
        Ok(builder.finish().into_series())
    }

    wkb_array
        .iter()
        .map(|wkb| wkb.map(|wkb| get_coordinates(wkb, dimension)).transpose())
        .collect()
}

pub fn flip_coordinates(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .transform_xy(|x, y| Some((y, x)))?
            .to_ewkb()
    })
}

pub fn get_point_n(wkb: &BinaryChunked, index: &UInt32Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise(wkb, index, |wkb, index| {
        if let (Some(wkb), Some(index)) = (wkb, index) {
            let index = index as usize;
            let geom = Geometry::new_from_wkb(wkb)?;
            let num_points = geom.get_num_points()?;
            if index < num_points {
                return Some(geom.get_point_n(index)?.to_ewkb()).transpose();
            }
        }
        Ok(None)
    })
}

pub fn get_interior_ring_n(wkb: &BinaryChunked, index: &UInt32Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise(wkb, index, |wkb, index| {
        if let (Some(wkb), Some(index)) = (wkb, index) {
            let geom = Geometry::new_from_wkb(wkb)?;
            let index = index as usize;
            let num_rings = geom.get_num_interior_rings()?;
            if index < num_rings {
                return Some(geom.get_interior_ring_n(index)?.to_ewkb()).transpose();
            }
        }
        Ok(None)
    })
}

pub fn get_geometry_n(wkb: &BinaryChunked, index: &UInt32Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise(wkb, index, |wkb, index| {
        if let (Some(wkb), Some(index)) = (wkb, index) {
            let index = index as usize;
            let geom = Geometry::new_from_wkb(wkb)?;
            let num_geom = geom.get_num_geometries()?;
            if index < num_geom {
                return Some(geom.get_geometry_n(index)?.to_ewkb()).transpose();
            }
        }
        Ok(None)
    })
}

pub fn get_parts(wkb: &BinaryChunked) -> GResult<ListChunked> {
    // TODO: use try_apply_nonnull_values_generic once pola-rs/polars#22233 is merged
    let dt = DataType::List(Box::new(DataType::Binary));
    try_unary_elementwise_values_with_dtype(wkb, dt, |wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        let num_geom = geom.get_num_geometries()?;
        let parts = BinaryViewArray::try_arr_from_iter((0..num_geom).map(|n| {
            let part = geom.get_geometry_n(n)?;
            part.to_ewkb()
        }))?;
        Ok(Box::new(parts) as Box<dyn Array>)
    })
}

pub fn get_precision(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.get_precision())
}

pub fn set_precision(
    wkb: &BinaryChunked,
    grid_size: &Float64Chunked,
    params: &SetPrecisionKwargs,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, grid_size, |wkb, grid_size| {
        Geometry::new_from_wkb(wkb)?
            .set_precision(grid_size, params.mode.into())?
            .to_ewkb()
    })
}

pub fn to_wkt(wkb: &BinaryChunked, params: &ToWktKwargs) -> GResult<StringChunked> {
    let mut writer = WKTWriter::new()?;
    if let Some(rounding_precision) = params.rounding_precision {
        writer.set_rounding_precision(rounding_precision);
    }
    writer.set_old_3D(params.old_3d);
    writer.set_trim(params.trim);
    writer.set_output_dimension(params.output_dimension.try_into()?);
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        writer.write(&geom)
    })
}

pub fn to_ewkt(wkb: &BinaryChunked, params: &ToWktKwargs) -> GResult<StringChunked> {
    let mut writer = WKTWriter::new()?;
    if let Some(rounding_precision) = params.rounding_precision {
        writer.set_rounding_precision(rounding_precision);
    }
    writer.set_old_3D(params.old_3d);
    writer.set_trim(params.trim);
    writer.set_output_dimension(params.output_dimension.try_into()?);
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.get_srid()? {
            0 => writer.write(&geom),
            srid => writer.write(&geom).map(|s| format!("SRID={srid};{s}")),
        }
    })
}

pub fn to_wkb(wkb: &BinaryChunked, params: &ToWkbKwargs) -> GResult<BinaryChunked> {
    let mut writer = WKBWriter::new()?;
    if let Some(byte_order) = params.byte_order {
        writer.set_wkb_byte_order(byte_order.try_into()?);
    }
    writer.set_include_SRID(params.include_srid);
    writer.set_output_dimension(params.output_dimension.try_into()?);
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        let res: Vec<u8> = writer.write_wkb(&geom)?.into();
        Ok(res)
    })
}

pub fn to_geojson(wkb: &BinaryChunked, params: &ToGeoJsonKwargs) -> GResult<StringChunked> {
    let mut writer = GeoJSONWriter::new()?;
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        writer.write_formatted(&geom, params.indent.unwrap_or(-1))
    })
}

pub fn to_python_dict(wkb: &BinaryChunked, py: Python) -> GResult<Vec<Option<PyObject>>> {
    let json = PyModule::import(py, "json").expect("Failed to load json");
    let loads = json.getattr("loads").expect("Failed to get json.loads");
    wkb.into_iter()
        .map(|wkb| {
            wkb.map(|wkb| {
                Geometry::new_from_wkb(wkb)
                    .and_then(|geom| geom.to_geojson())
                    .map(|json| loads.call1((json,)).expect("Invalid GeoJSON").into())
            })
            .transpose()
        })
        .collect::<GResult<Vec<Option<PyObject>>>>()
}

pub fn cast(wkb: &BinaryChunked, into: &UInt32Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, into, |wkb, into| {
        let into: WKBGeometryType = into.try_into().unwrap();
        let into: GeometryTypes = into.try_into()?;
        Geometry::new_from_wkb(wkb)?.cast(into)?.to_ewkb()
    })
}

pub fn multi(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.geometry_type() {
            Point => geom.cast(MultiPoint),
            LineString => geom.cast(MultiLineString),
            CircularString => geom.cast(MultiCurve),
            Polygon => geom.cast(MultiPolygon),
            CurvePolygon => geom.cast(MultiSurface),
            _ => Ok(Geom::clone(&geom)),
        }?
        .to_ewkb()
    })
}

pub fn area(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.area())
}

pub fn bounds(wkb: &BinaryChunked) -> GResult<ArrayChunked> {
    let dt = DataType::Array(Box::new(DataType::Float64), 4);
    try_unary_elementwise_values_with_dtype(wkb, dt, |wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        let bounds = if geom.is_empty()? {
            [f64::NAN, f64::NAN, f64::NAN, f64::NAN]
        } else {
            let x_min = geom.get_x_min()?;
            let y_min = geom.get_y_min()?;
            let x_max = geom.get_x_max()?;
            let y_max = geom.get_y_max()?;
            [x_min, y_min, x_max, y_max]
        };
        Ok(Box::new(Float64Array::from_slice(bounds)) as Box<dyn Array>)
    })
}

pub fn length(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.length())
}

pub fn distance(a: &BinaryChunked, b: &BinaryChunked) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        if a.is_empty()? || b.is_empty()? {
            Ok(f64::NAN) // Match `hausdorff_distance` and `frechet_distance` behavior
        } else {
            a.distance(&b)
        }
    })
}

pub fn hausdorff_distance(a: &BinaryChunked, b: &BinaryChunked) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        a.hausdorff_distance(&b)
    })
}

pub fn hausdorff_distance_densify(
    a: &BinaryChunked,
    b: &BinaryChunked,
    densify: f64,
) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        a.hausdorff_distance_densify(&b, densify)
    })
}

pub fn frechet_distance(a: &BinaryChunked, b: &BinaryChunked) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        // TODO: bug report to GEOS
        if a.is_empty()? || b.is_empty()? {
            Ok(f64::NAN)
        } else {
            a.frechet_distance(&b)
        }
    })
}

pub fn frechet_distance_densify(
    a: &BinaryChunked,
    b: &BinaryChunked,
    densify: f64,
) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        // TODO: bug report to GEOS
        if a.is_empty()? || b.is_empty()? {
            Ok(f64::NAN)
        } else {
            a.frechet_distance_densify(&b, densify)
        }
    })
}

pub fn minimum_clearance(wkb: &BinaryChunked) -> GResult<Float64Chunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.minimum_clearance())
}

pub fn has_z(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.has_z())
}

pub fn has_m(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.has_m())
}

pub fn is_ccw(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.geometry_type() {
            Point | LinearRing | LineString | CircularString => geom.get_coord_seq()?.is_ccw(),
            _ => Ok(false),
        }
    })
}

pub fn is_closed(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.geometry_type() {
            LinearRing | LineString | CircularString | MultiLineString | MultiCurve => {
                geom.is_closed()
            }
            _ => Ok(false),
        }
    })
}

pub fn is_empty(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.is_empty())
}

pub fn is_ring(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.is_ring())
}

pub fn is_simple(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.is_simple())
}

pub fn is_valid(wkb: &BinaryChunked) -> GResult<BooleanChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Ok(Geometry::new_from_wkb(wkb)?.is_valid()))
}

pub fn is_valid_reason(wkb: &BinaryChunked) -> GResult<StringChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.is_valid_reason())
}

pub fn crosses(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::crosses(&a, &b)
    })
}

pub fn contains(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::contains(&a, &b)
    })
}

pub fn contains_properly(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        let prepared = a.to_prepared_geom()?;
        prepared.contains_properly(&b)
    })
}

pub fn covered_by(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::covered_by(&a, &b)
    })
}

pub fn covers(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::covers(&a, &b)
    })
}

pub fn disjoint(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::disjoint(&a, &b)
    })
}

pub fn dwithin(a: &BinaryChunked, b: &BinaryChunked, distance: f64) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::distance(&a, &b).map(|d| d < distance)
    })
}

pub fn intersects(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::intersects(&a, &b)
    })
}

pub fn overlaps(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::overlaps(&a, &b)
    })
}

pub fn touches(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::touches(&a, &b)
    })
}

pub fn within(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::within(&a, &b)
    })
}

pub fn equals(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::equals(&a, &b)
    })
}

pub fn equals_identical(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::equals_identical(&a, &b)
    })
}

pub fn equals_exact(
    a: &BinaryChunked,
    b: &BinaryChunked,
    tolerance: f64,
) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::equals_exact(&a, &b, tolerance)
    })
}

pub fn relate(a: &BinaryChunked, b: &BinaryChunked) -> GResult<StringChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::relate(&a, &b)
    })
}

pub fn relate_pattern(
    a: &BinaryChunked,
    b: &BinaryChunked,
    pattern: &str,
) -> GResult<BooleanChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::relate_pattern(&a, &b, pattern)
    })
}

pub fn intersects_xy(
    wkb: &BinaryChunked,
    x: &Float64Chunked,
    y: &Float64Chunked,
) -> GResult<BooleanChunked> {
    try_ternary_elementwise_values(wkb, x, y, |wkb, x, y| {
        Geometry::new_from_wkb(wkb)?
            .to_prepared_geom()?
            .intersects_xy(x, y)
    })
}

pub fn contains_xy(
    wkb: &BinaryChunked,
    x: &Float64Chunked,
    y: &Float64Chunked,
) -> GResult<BooleanChunked> {
    try_ternary_elementwise_values(wkb, x, y, |wkb, x, y| {
        Geometry::new_from_wkb(wkb)?
            .to_prepared_geom()?
            .contains_xy(x, y)
    })
}

pub fn difference(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::difference(&a, &b)?.to_ewkb()
    })
}

pub fn difference_prec(
    a: &BinaryChunked,
    b: &BinaryChunked,
    grid_size: f64,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::difference_prec(&a, &b, grid_size)?.to_ewkb()
    })
}

pub fn intersection(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::intersection(&a, &b)?.to_ewkb()
    })
}

pub fn intersection_prec(
    a: &BinaryChunked,
    b: &BinaryChunked,
    grid_size: f64,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::intersection_prec(&a, &b, grid_size)?.to_ewkb()
    })
}

pub fn sym_difference(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::sym_difference(&a, &b)?.to_ewkb()
    })
}

pub fn sym_difference_prec(
    a: &BinaryChunked,
    b: &BinaryChunked,
    grid_size: f64,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::sym_difference_prec(&a, &b, grid_size)?.to_ewkb()
    })
}

pub fn unary_union(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?.unary_union()?.to_ewkb()
    })
}

pub fn unary_union_prec(wkb: &BinaryChunked, grid_size: f64) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .unary_union_prec(grid_size)?
            .to_ewkb()
    })
}

pub fn disjoint_subset_union(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .disjoint_subset_union()?
            .to_ewkb()
    })
}

pub fn union(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::union(&a, &b)?.to_ewkb()
    })
}

pub fn union_prec(a: &BinaryChunked, b: &BinaryChunked, grid_size: f64) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::union_prec(&a, &b, grid_size)?.to_ewkb()
    })
}

pub fn coverage_union(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if matches!(
            geom.geometry_type(),
            MultiPoint
                | MultiLineString
                | MultiCurve
                | MultiPolygon
                | MultiSurface
                | GeometryCollection
        ) {
            geom.coverage_union()?.to_ewkb()
        } else {
            let msg = "Geometry must be a collection";
            Err(geos::Error::GenericError(msg.into()))
        }
    })
}

fn collect_geometry_vec(wkb: &BinaryChunked) -> GResult<Vec<Geometry>> {
    wkb.into_iter()
        .flatten()
        .map(Geometry::new_from_wkb)
        .collect()
}

pub fn coverage_union_all(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    collect_geometry_vec(wkb)
        .and_then(Geometry::create_geometry_collection)
        .and_then(|geom| geom.coverage_union())
        .and_then(|geom| geom.to_ewkb())
        .map(|res| BinaryChunked::from_slice(wkb.name().clone(), &[res]))
}

pub fn polygonize(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    collect_geometry_vec(wkb)
        .and_then(|vec| Geometry::polygonize(&vec))
        .and_then(|geom| geom.to_ewkb())
        .map(|res| BinaryChunked::from_slice(wkb.name().clone(), &[res]))
}

fn aggregate_with<F>(wkb: &BinaryChunked, func: F) -> GResult<BinaryChunked>
where
    F: FnOnce(Vec<Geometry>) -> GResult<Geometry>,
{
    collect_geometry_vec(wkb)
        .and_then(func)
        .and_then(|geom| geom.to_ewkb())
        .map(|res| BinaryChunked::from_slice(wkb.name().clone(), &[res]))
}

fn collection_supertype(wkb: &BinaryChunked) -> GResult<GeometryTypes> {
    let geometry_types: Vec<GeometryTypes> = get_type_id(wkb)?
        .unique()
        .unwrap()
        .sort(false)
        .into_iter()
        .flatten()
        .map(WKBGeometryType::try_from)
        .map(Result::unwrap)
        .map(TryInto::try_into)
        .collect::<Result<_, _>>()?;
    Ok(match &geometry_types.as_slice() {
        &[Point] => MultiPoint,
        &[LineString] => MultiLineString,
        &[CircularString]
        | &[CompoundCurve]
        | &[LineString, CircularString]
        | &[LineString, CircularString, CompoundCurve] => MultiCurve,
        &[Polygon] => MultiPolygon,
        &[CurvePolygon] | &[Polygon, CurvePolygon] => MultiSurface,
        _ => GeometryCollection,
    })
}

pub fn collect(wkb: &BinaryChunked, into: Option<WKBGeometryType>) -> GResult<BinaryChunked> {
    let into = match into {
        Some(into) => into.try_into(),
        None => collection_supertype(wkb),
    }?;
    match into {
        MultiPoint => aggregate_with(wkb, Geometry::create_multipoint),
        MultiLineString => aggregate_with(wkb, Geometry::create_multiline_string),
        MultiCurve => aggregate_with(wkb, Geometry::create_multicurve),
        MultiPolygon => aggregate_with(wkb, Geometry::create_multipolygon),
        MultiSurface => aggregate_with(wkb, Geometry::create_multisurface),
        GeometryCollection => aggregate_with(wkb, Geometry::create_geometry_collection),
        _ => Err(geos::Error::GenericError(
            "type must be a collection".into(),
        )),
    }
}

pub fn boundary(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        match geom.geometry_type() {
            GeometryCollection => Geometry::create_empty_collection(GeometryCollection),
            _ => geom.boundary(),
        }?
        .to_ewkb()
    })
}

pub fn buffer(
    wkb: &BinaryChunked,
    distance: &Float64Chunked,
    params: &BufferKwargs,
) -> GResult<BinaryChunked> {
    let buffer_params: BufferParams = params.try_into()?;
    broadcast_try_binary_elementwise_values(wkb, distance, |wkb, distance| {
        Geometry::new_from_wkb(wkb)?
            .buffer_with_params(distance, &buffer_params)?
            .to_ewkb()
    })
}

pub fn offset_curve(
    wkb: &BinaryChunked,
    distance: &Float64Chunked,
    params: &OffsetCurveKwargs,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, distance, |wkb, distance| {
        Geometry::new_from_wkb(wkb)?
            .offset_curve(
                distance,
                params.quad_segs,
                params.join_style.into(),
                params.mitre_limit,
            )?
            .to_ewkb()
    })
}

pub fn get_centroid(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?.get_centroid()?.to_ewkb()
    })
}

pub fn get_center(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return Geometry::create_empty_point()?.to_ewkb();
        }
        let x = f64::midpoint(geom.get_x_min()?, geom.get_x_max()?);
        let y = f64::midpoint(geom.get_y_min()?, geom.get_y_max()?);
        Geometry::create_point(CoordSeq::new_from_buffer(&[x, y], 1, false, false)?)?.to_ewkb()
    })
}

pub fn clip_by_rect(wkb: &BinaryChunked, rect: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, rect, |wkb, rect| {
        let rect = unsafe { rect.as_any().downcast_ref_unchecked::<Float64Array>() };
        let xmin = unsafe { rect.get_unchecked(0) }.unwrap_or(f64::NAN);
        let ymin = unsafe { rect.get_unchecked(1) }.unwrap_or(f64::NAN);
        let xmax = unsafe { rect.get_unchecked(2) }.unwrap_or(f64::NAN);
        let ymax = unsafe { rect.get_unchecked(3) }.unwrap_or(f64::NAN);
        Geometry::new_from_wkb(wkb)?
            .clip_by_rect(xmin, ymin, xmax, ymax)?
            .to_ewkb()
    })
}

pub fn convex_hull(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?.convex_hull()?.to_ewkb()
    })
}

pub fn concave_hull(wkb: &BinaryChunked, params: &ConcaveHullKwargs) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .concave_hull(params.ratio, params.allow_holes)?
            .to_ewkb()
    })
}

pub fn delaunay_triangulation(
    wkb: &BinaryChunked,
    params: &DelaunayTrianlesKwargs,
) -> GResult<BinaryChunked> {
    collect_geometry_vec(wkb)
        .and_then(Geometry::create_geometry_collection)
        .and_then(|geom| geom.delaunay_triangulation(params.tolerance, params.only_edges))
        .and_then(|geom| geom.to_ewkb())
        .map(|res| BinaryChunked::from_slice(wkb.name().clone(), &[res]))
}

pub fn densify(wkb: &BinaryChunked, tolerance: &Float64Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, tolerance, |wkb, tolerance| {
        Geometry::new_from_wkb(wkb)?.densify(tolerance)?.to_ewkb()
    })
}

pub fn envelope(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.envelope()?.to_ewkb())
}

pub fn extract_unique_points(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .extract_unique_points()?
            .to_ewkb()
    })
}

pub fn build_area(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.build_area()?.to_ewkb())
}

pub fn make_valid(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.make_valid()?.to_ewkb())
}

pub fn normalize(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let mut geom = Geometry::new_from_wkb(wkb)?;
        geom.normalize()?;
        geom.to_ewkb()
    })
}

pub fn node(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.node()?.to_ewkb())
}

pub fn point_on_surface(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?.point_on_surface()?.to_ewkb()
    })
}

pub fn remove_repeated_points(
    wkb: &BinaryChunked,
    tolerance: &Float64Chunked,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, tolerance, |wkb, tolerance| {
        Geometry::new_from_wkb(wkb)?
            .remove_repeated_points(tolerance)?
            .to_ewkb()
    })
}

pub fn reverse(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.reverse()?.to_ewkb())
}

pub fn simplify(wkb: &BinaryChunked, tolerance: &Float64Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, tolerance, |wkb, tolerance| {
        Geometry::new_from_wkb(wkb)?.simplify(tolerance)?.to_ewkb()
    })
}

pub fn topology_preserve_simplify(
    wkb: &BinaryChunked,
    tolerance: &Float64Chunked,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, tolerance, |wkb, tolerance| {
        Geometry::new_from_wkb(wkb)?
            .topology_preserve_simplify(tolerance)?
            .to_ewkb()
    })
}

pub fn force_2d(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            let mut res = match geom.geometry_type() {
                Point => Geometry::create_empty_point(),
                LineString | LinearRing => Geometry::create_empty_line_string(),
                Polygon => Geometry::create_empty_polygon(),
                MultiPoint => Geometry::create_empty_collection(MultiPoint),
                MultiLineString => Geometry::create_empty_collection(MultiLineString),
                MultiPolygon => Geometry::create_empty_collection(MultiPolygon),
                GeometryCollection => Geometry::create_empty_collection(GeometryCollection),
                CircularString => Geometry::create_empty_circular_string(),
                CompoundCurve => Geometry::create_empty_compound_curve(),
                CurvePolygon => Geometry::create_empty_curve_polygon(),
                MultiCurve => Geometry::create_empty_collection(MultiCurve),
                MultiSurface => Geometry::create_empty_collection(MultiSurface),
            }?;
            res.set_srid(geom.get_srid()?);
            res
        } else {
            geom.transform_xyz(|x, y, _z| Some((x, y, f64::NAN)))?
        }
        .to_ewkb()
    })
}

pub fn force_3d(wkb: &BinaryChunked, z: &Float64Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, z, |wkb, new_z| {
        Geometry::new_from_wkb(wkb)?
            .transform_xyz(|x, y, z| Some((x, y, if z.is_nan() { new_z } else { z })))?
            .to_ewkb()
    })
}

pub fn minimum_rotated_rectangle(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .minimum_rotated_rectangle()?
            .to_ewkb()
    })
}

pub fn translate(wkb: &BinaryChunked, factors: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        geom.translate(x, y, z)?.to_ewkb()
    })
}

pub fn rotate_around_centroid(
    wkb: &BinaryChunked,
    angle: &Float64Chunked,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, angle, |wkb, angle| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let centroid = geom.get_centroid()?;
        let x0 = centroid.get_x()?;
        let y0 = centroid.get_y()?;
        geom.rotate(angle, x0, y0)?.to_ewkb()
    })
}

pub fn rotate_around_center(wkb: &BinaryChunked, angle: &Float64Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, angle, |wkb, angle| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let x0 = f64::midpoint(geom.get_x_min()?, geom.get_x_max()?);
        let y0 = f64::midpoint(geom.get_y_min()?, geom.get_y_max()?);
        geom.rotate(angle, x0, y0)?.to_ewkb()
    })
}

pub fn rotate_around_point(
    wkb: &BinaryChunked,
    angle: &Float64Chunked,
    origin: &(f64, f64),
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, angle, |wkb, angle| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        geom.rotate(angle, origin.0, origin.1)?.to_ewkb()
    })
}

pub fn scale_from_centroid(wkb: &BinaryChunked, factors: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        let centroid = geom.get_centroid()?;
        let x0 = centroid.get_x()?;
        let y0 = centroid.get_y()?;
        let z0 = centroid.get_z()?;
        geom.scale(x, y, z, x0, y0, z0)?.to_ewkb()
    })
}

pub fn scale_from_center(wkb: &BinaryChunked, factors: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        let x0 = f64::midpoint(geom.get_x_min()?, geom.get_x_max()?);
        let y0 = f64::midpoint(geom.get_y_min()?, geom.get_y_max()?);
        let z0 = 0.0;
        geom.scale(x, y, z, x0, y0, z0)?.to_ewkb()
    })
}

pub fn scale_from_point(
    wkb: &BinaryChunked,
    factors: &ArrayChunked,
    origin: &(f64, f64, f64),
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        geom.scale(x, y, z, origin.0, origin.1, origin.2)?.to_ewkb()
    })
}

pub fn skew_from_centroid(wkb: &BinaryChunked, factors: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        let centroid = geom.get_centroid()?;
        let x0 = centroid.get_x()?;
        let y0 = centroid.get_y()?;
        let z0 = centroid.get_z()?;
        geom.skew(x, y, z, x0, y0, z0)?.to_ewkb()
    })
}

pub fn skew_from_center(wkb: &BinaryChunked, factors: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        let x0 = f64::midpoint(geom.get_x_min()?, geom.get_x_max()?);
        let y0 = f64::midpoint(geom.get_y_min()?, geom.get_y_max()?);
        let z0 = 0.0;
        geom.skew(x, y, z, x0, y0, z0)?.to_ewkb()
    })
}

pub fn skew_from_point(
    wkb: &BinaryChunked,
    factors: &ArrayChunked,
    origin: &(f64, f64, f64),
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, factors, |wkb, factors| {
        let geom = Geometry::new_from_wkb(wkb)?;
        if geom.is_empty()? {
            return geom.to_ewkb();
        }
        let factors = unsafe { factors.as_any().downcast_ref_unchecked::<Float64Array>() };
        let x = unsafe { factors.get_unchecked(0) }.unwrap_or(f64::NAN);
        let y = unsafe { factors.get_unchecked(1) }.unwrap_or(f64::NAN);
        let z = unsafe { factors.get_unchecked(2) }.unwrap_or(f64::NAN);
        geom.skew(x, y, z, origin.0, origin.1, origin.2)?.to_ewkb()
    })
}

pub fn affine_transform_2d(wkb: &BinaryChunked, matrix: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, matrix, |wkb, matrix| {
        let matrix = unsafe { matrix.as_any().downcast_ref_unchecked::<Float64Array>() };
        Geometry::new_from_wkb(wkb)?
            .apply_affine_transform(
                unsafe { matrix.get_unchecked(0) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(1) }.unwrap_or(f64::NAN),
                0.0,
                unsafe { matrix.get_unchecked(2) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(3) }.unwrap_or(f64::NAN),
                0.0,
                0.0,
                0.0,
                1.0,
                unsafe { matrix.get_unchecked(4) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(5) }.unwrap_or(f64::NAN),
                0.0,
            )?
            .to_ewkb()
    })
}

pub fn affine_transform_3d(wkb: &BinaryChunked, matrix: &ArrayChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, matrix, |wkb, matrix| {
        let matrix = unsafe { matrix.as_any().downcast_ref_unchecked::<Float64Array>() };
        Geometry::new_from_wkb(wkb)?
            .apply_affine_transform(
                unsafe { matrix.get_unchecked(0) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(1) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(2) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(3) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(4) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(5) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(6) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(7) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(8) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(9) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(10) }.unwrap_or(f64::NAN),
                unsafe { matrix.get_unchecked(11) }.unwrap_or(f64::NAN),
            )?
            .to_ewkb()
    })
}

pub fn interpolate(wkb: &BinaryChunked, distance: &Float64Chunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, distance, |wkb, distance| {
        Geometry::new_from_wkb(wkb)?
            .interpolate(distance)?
            .to_ewkb()
    })
}

pub fn interpolate_normalized(
    wkb: &BinaryChunked,
    distance: &Float64Chunked,
) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(wkb, distance, |wkb, distance| {
        Geometry::new_from_wkb(wkb)?
            .interpolate_normalized(distance)?
            .to_ewkb()
    })
}

pub fn project(a: &BinaryChunked, b: &BinaryChunked) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        // Empty lines error, empty points segfault
        if a.geometry_type() == LineString && a.is_empty()? || b.is_empty()? {
            Ok(f64::NAN)
        } else {
            a.project(&b)
        }
    })
}

pub fn project_normalized(a: &BinaryChunked, b: &BinaryChunked) -> GResult<Float64Chunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        // Empty lines error, empty points segfault
        if a.geometry_type() == LineString && a.is_empty()? || b.is_empty()? {
            Ok(f64::NAN)
        } else {
            a.project_normalized(&b)
        }
    })
}

pub fn line_merge(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| Geometry::new_from_wkb(wkb)?.line_merge()?.to_ewkb())
}

pub fn line_merge_directed(wkb: &BinaryChunked) -> GResult<BinaryChunked> {
    wkb.try_apply_nonnull_values_generic(|wkb| {
        Geometry::new_from_wkb(wkb)?
            .line_merge_directed()?
            .to_ewkb()
    })
}

pub fn shared_paths(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        a.shared_paths(&b)?.to_ewkb()
    })
}

pub fn shortest_line(a: &BinaryChunked, b: &BinaryChunked) -> GResult<BinaryChunked> {
    broadcast_try_binary_elementwise_values(a, b, |a, b| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        if a.is_empty()? || b.is_empty()? {
            Geometry::create_empty_line_string()?
        } else {
            let seq = a.nearest_points(&b)?;
            Geometry::create_line_string(seq)?
        }
        .to_ewkb()
    })
}

pub fn snap(
    a: &BinaryChunked,
    b: &BinaryChunked,
    tolerance: &Float64Chunked,
) -> GResult<BinaryChunked> {
    broadcast_try_ternary_elementwise_values(a, b, tolerance, |a, b, tolerance| {
        let a = Geometry::new_from_wkb(a)?;
        let b = Geometry::new_from_wkb(b)?;
        Geometry::snap(&a, &b, tolerance)?.to_ewkb()
    })
}

pub fn voronoi_polygons(wkb: &BinaryChunked, params: &VoronoiKwargs) -> GResult<BinaryChunked> {
    let extend_to = params
        .extend_to
        .as_ref()
        .map(|wkb| Geometry::new_from_wkb(wkb))
        .transpose()?;
    collect_geometry_vec(wkb)
        .and_then(Geometry::create_geometry_collection)
        .and_then(|geom| geom.voronoi(extend_to.as_ref(), params.tolerance, params.only_edges))
        .and_then(|geom| geom.to_ewkb())
        .map(|res| BinaryChunked::from_slice(wkb.name().clone(), &[res]))
}

fn strtree(geoms: &[Option<Geometry>]) -> GResult<STRtree<usize>> {
    let length = geoms.len();
    geoms.iter().enumerate().try_fold(
        STRtree::<usize>::with_capacity(length)?,
        |mut tree, (index, geom)| {
            if let Some(geom) = geom {
                tree.insert(geom, index);
            }
            Ok(tree)
        },
    )
}

pub fn sjoin(
    left: &BinaryChunked,
    right: &BinaryChunked,
    predicate: SpatialJoinPredicate,
) -> GResult<(UInt32Chunked, UInt32Chunked)> {
    let predicate = match predicate {
        SpatialJoinPredicate::IntersectsBbox => |_: &_, _: &_| Ok(true),
        SpatialJoinPredicate::Intersects => PreparedGeometry::intersects,
        SpatialJoinPredicate::Within => PreparedGeometry::within,
        SpatialJoinPredicate::Contains => PreparedGeometry::contains,
        SpatialJoinPredicate::Overlaps => PreparedGeometry::overlaps,
        SpatialJoinPredicate::Crosses => PreparedGeometry::crosses,
        SpatialJoinPredicate::Touches => PreparedGeometry::touches,
        SpatialJoinPredicate::Covers => PreparedGeometry::covers,
        SpatialJoinPredicate::CoveredBy => PreparedGeometry::covered_by,
        SpatialJoinPredicate::ContainsProperly => PreparedGeometry::contains_properly,
    };
    let left_geoms = left
        .into_iter()
        .map(|v| v.map(Geometry::new_from_wkb).transpose())
        .collect::<GResult<Vec<_>>>()?;
    let spatial_index = strtree(&left_geoms)?;
    let left_geoms = left_geoms
        .iter()
        .map(|v| v.as_ref().map(Geom::to_prepared_geom).transpose())
        .collect::<GResult<Vec<_>>>()?;

    let builder_len = core::cmp::max(left.len(), right.len());
    let mut left_index_builder =
        PrimitiveChunkedBuilder::<UInt32Type>::new("left_index".into(), builder_len);
    let mut right_index_builder =
        PrimitiveChunkedBuilder::<UInt32Type>::new("right_index".into(), builder_len);

    for (right_index, wkb) in right.into_iter().enumerate() {
        if wkb.is_none() {
            continue;
        }
        let right_geom = Geometry::new_from_wkb(wkb.unwrap())?;
        spatial_index.query(&right_geom, |left_index| {
            let left_geom = left_geoms[*left_index]
                .as_ref()
                .expect("Shouldn't be able to match None");
            if matches!(predicate(left_geom, &right_geom), Ok(true)) {
                left_index_builder.append_value(*left_index as u32);
                right_index_builder.append_value(right_index as u32);
            }
        });
    }
    Ok((left_index_builder.finish(), right_index_builder.finish()))
}

fn apply_proj_transform(src: &Proj, dst: &Proj, geom: &Geometry) -> GResult<Geometry> {
    let global_success = RefCell::new(Ok(()));

    let transformed = geom.transform_xyz(|x, y, z| {
        let mut success = Ok(());
        let has_z = !z.is_nan();
        let mut new_x: f64;
        let mut new_y: f64;
        let mut new_z: f64;

        if src.is_latlong() {
            new_x = x.to_radians();
            new_y = y.to_radians();
            new_z = z.to_radians();
        } else {
            new_x = x;
            new_y = y;
            new_z = z;
        }
        if has_z {
            match proj4rs::adaptors::transform_xyz(src, dst, new_x, new_y, new_z) {
                Ok(transformed) => (new_x, new_y, new_z) = transformed,
                Err(e) => success = Err(e),
            }
        } else {
            match proj4rs::adaptors::transform_xy(src, dst, new_x, new_y) {
                Ok(transformed) => (new_x, new_y) = transformed,
                Err(e) => success = Err(e),
            }
        }
        if dst.is_latlong() {
            new_x = new_x.to_degrees();
            new_y = new_y.to_degrees();
            new_z = new_z.to_degrees();
        }
        if let Ok(()) = success {
            Some((new_x, new_y, new_z))
        } else {
            let _ = global_success.replace(success);
            None
        }
    });
    match global_success.into_inner() {
        Ok(()) => transformed,
        Err(e) => Err(geos::Error::GenericError(e.to_string())),
    }
}
struct ProjCache(HashMap<u16, Proj>);

impl ProjCache {
    fn new() -> Self {
        Self(HashMap::<u16, Proj>::new())
    }

    fn get(&mut self, srid: u16) -> Result<Proj, ProjError> {
        Ok(match self.0.entry(srid) {
            std::collections::hash_map::Entry::Occupied(proj) => proj.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => e.insert(Proj::from_epsg_code(srid)?),
        }
        .clone())
    }
}

pub fn to_srid(wkb: &BinaryChunked, srid: &Int64Chunked) -> GResult<BinaryChunked> {
    let mut cache = ProjCache::new();

    broadcast_try_binary_elementwise_values(wkb, srid, |wkb, dest_srid| {
        let geom = Geometry::new_from_wkb(wkb)?;
        let geom_srid = geom.get_srid()?;

        if i64::from(geom_srid) == dest_srid || geom.is_empty()? {
            return Ok(wkb.into());
        }

        let srid_err = |srid| geos::Error::GenericError(format!("Unknown SRID: {srid}"));

        let proj_src = geom_srid
            .try_into()
            .map(|geom_srid| cache.get(geom_srid))
            .map_err(|_| srid_err(geom_srid))?
            .map_err(|_| srid_err(geom_srid))?;

        let proj_dst = dest_srid
            .try_into()
            .map(|dest_srid| cache.get(dest_srid))
            .map_err(|_| srid_err(geom_srid))?
            .map_err(|_| srid_err(geom_srid))?;

        let mut transformed = apply_proj_transform(&proj_src, &proj_dst, &geom)?;
        transformed.set_srid(dest_srid as _);
        transformed.to_ewkb()
    })
}
