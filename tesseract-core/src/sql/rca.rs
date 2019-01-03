// Optimization for RCA
// Ordinarily, just a, b, c, and d are scanned separately and then combined by joins.
// a: (each product, each city) // can be cut on drill 1
// b: (all products, each city)
// c: (each product, all cities) // can be cut on drill 1
// d: (all products, all cities)
//
// Note that external cuts are always valid (i.e. if above abcd were cut by a year).
//
// However, this results in extra scans, especially if there's no internal cuts (cuts on an rca
// drill dim).
//
// The optimization is to derive the c and d aggregates from a and b. Since cuts are allowed on the
// first drill in the rca, both a and b have to be scanned (b cannot be cut on the first drill).
//
// In clickhouse there is no partition, so it's trickier to do what looks like two different group
// by.
//
// The general idea is to do one group by, in which both the measure and the 2nd drill are rolled
// up.
// - measure is rolled up by aggregate fn (e.g. sum)
// - 2nd drill is rolled up by groupArray, which just collects all the values into an array in
// order.
// - the original measure is also rolled up by groupArray.
//
// Then the pivoted table is melted using Array Join on the 2nd drill and the original measure
// (which would be a or c), while preserving the aggregated measure (c or d) from the pivoted
// table.
//
// An example (not accounting for external cuts or dims) would be
// select drill_1_id, drill_2_id, a, c from (
//   select drill_1_id, groupArray(drill_2_id) as drill_2_id_s, groupArray(a) a_s, sum(a) as c from (
//     select * from a_table
//   )
//   group by drill_1_id
// )
// array join drill_2_id_s as drill_2_id, a_s as a

use itertools::join;

use crate::sql::primary_agg::primary_agg;
use super::{
    TableSql,
    CutSql,
    DrilldownSql,
    MeasureSql,
    RcaSql,
};

pub fn calculate(
    table: &TableSql,
    cuts: &[CutSql],
    drills: &[DrilldownSql],
    meas: &[MeasureSql],
    rca: &RcaSql,
    ) -> (String, String)
{
    // append the correct rca drill to drilldowns
    // for a, both
    // for b, d2
    // for c, d1
    // for d, none
    let mut a_drills = drills.to_vec();
    let mut b_drills = drills.to_vec();
    let mut c_drills = drills.to_vec();
    let     d_drills = drills.to_vec();

    a_drills.extend_from_slice(&rca.drill_1);
    a_drills.extend_from_slice(&rca.drill_2);

    b_drills.extend_from_slice(&rca.drill_2);

    c_drills.extend_from_slice(&rca.drill_1);

    println!("a: {:?}", a_drills);
    println!("b: {:?}", b_drills);
    println!("c: {:?}", c_drills);
    println!("d: {:?}", d_drills);

    // prepend the rca sql to meas
    let all_meas = {
        let mut temp = vec![rca.mea.clone()];
        temp.extend_from_slice(meas);
        temp
    };

    // for cuts,
    // - a can be cut on d1 and ext
    // - b cannot be int cut, only ext
    // - c can be cut on d1 and ext
    // - d cannot be int cut, only ext
    //
    // In the future, would I allow more cuts? Maybe depending on use case
    //
    // The blacklist is the drilldowns contained in each of a, b, c, d

    let ac_drill_keys_blacklist: Vec<_> = rca.drill_2.iter()
        .flat_map(|d| d.level_columns.iter().map(|l| l.key_column.clone()))
        .collect();

    let bd_drill_keys_blacklist: Vec<_> = rca.drill_1.iter().chain(rca.drill_2.iter())
        .flat_map(|d| d.level_columns.iter().map(|l| l.key_column.clone()))
        .collect();

    let ac_cuts: Vec<_> = cuts.iter()
        .filter(|cut| {
            ac_drill_keys_blacklist.iter().find(|k| **k == cut.column).is_none()
        })
        .cloned()
        .collect();

    let bd_cuts: Vec<_> = cuts.iter()
        .filter(|cut| {
            bd_drill_keys_blacklist.iter().find(|k| **k == cut.column).is_none()
        })
        .cloned()
        .collect();

    println!("{:#?}", cuts);
    println!("{:#?}", ac_cuts);
    println!("{:#?}", bd_cuts);

    // now aggregate each component
    //
    // As an optimization, c is calculated from a, and d is calculated from b
    // If there's no internal cuts, then b, c, d are calculated from a.

    // First do aggregation for part a, b
    let (a, a_final_drills) = primary_agg(table, &ac_cuts, &a_drills, &all_meas);
    let (b, b_final_drills) = primary_agg(table, &bd_cuts, &b_drills, &all_meas);

    // replace final_m0 with letter name.
    // I put the rca measure at the beginning of the drills, so it should
    // always be m0
    let a = a.replace("final_m0", "a");
    let b = b.replace("final_m0", "b");

    // for clickhouse, need to make groupArray and Array Join clauses for drill_1 for when
    // aggregating a to c, and b to d.
    // (drill_2 would be needed if going from a to b)
    // TODO refacto these lines out to helpers
    let group_array_rca_drill_2 = rca.drill_2.iter()
        .flat_map(|d| d.level_columns.iter().map(|l| {
            if let Some(ref name_col) = l.name_column {
                format!("groupArray({key_col}) as {key_col}_s, groupArray({name_col}) as {name_col}_s", key_col=l.key_column, name_col=name_col)
            } else {
                format!("groupArray({col}) as {col}_s", col=l.key_column)
            }
        }));
    let group_array_rca_drill_2 = join(group_array_rca_drill_2, ", ");

    let join_array_rca_drill_2 = rca.drill_2.iter()
        .flat_map(|d| d.level_columns.iter().map(|l| {
            if let Some(ref name_col) = l.name_column {
                format!("{key_col}_s as {key_col}, {name_col}_s as {name_col}", key_col=l.key_column, name_col=name_col)
            } else {
                format!("{col}_s as {col}", col=l.key_column)
            }
        }));
    let join_array_rca_drill_2 = join(join_array_rca_drill_2, ", ");

    // groupArray cols (the drill_2 from rca) can't be included in the group by or select
    let c_drills_minus_rca_drill_2 = c_drills.iter()
        .filter(|d| !rca.drill_2.contains(&d))
        .map(|d| d.col_string());
    let c_drills_minus_rca_drill_2 = join(c_drills_minus_rca_drill_2, ", ");

    let d_drills_minus_rca_drill_2 = d_drills.iter()
        .filter(|d| !rca.drill_2.contains(&d))
        .map(|d| d.col_string());
    let d_drills_minus_rca_drill_2 = join(d_drills_minus_rca_drill_2, ", ");

    // a and c drills are kept as-is
    let a_drills_str = a_drills.iter()
        .map(|d| d.col_string());
    let a_drills_str = join(a_drills_str, ", ");

    let b_drills_str = b_drills.iter()
        .map(|d| d.col_string());
    let b_drills_str = join(b_drills_str, ", ");


    // Now add part c
    let ac = format!("select {}, a, c from \
                      (select {}, {}, groupArray(a) as a_s, sum(a) as c from ({}) group by {}) \
                      Array Join {}, a_s as a",
        a_drills_str,
        c_drills_minus_rca_drill_2,
        group_array_rca_drill_2,
        a,
        c_drills_minus_rca_drill_2,
        join_array_rca_drill_2,
    );
    println!("{}", ac);

    // Now add part d
    let bd = if d_drills.is_empty() {
            format!("select {}, b, d from \
                        (select groupArray(b) as b_s, sum(b) as d from ({})) \
                        Array Join {}, b_s as b",
            b_drills_str,
            b,
            join_array_rca_drill_2,
        )
    } else {
            format!("select {}, b, d from \
                        (select {}, {}, groupArray(b) as b_s, sum(b) as d from ({}) group by {}) \
                        Array Join {}, b_s as b",
            b_drills_str,
            d_drills_minus_rca_drill_2,
            group_array_rca_drill_2,
            b,
            d_drills_minus_rca_drill_2,
            join_array_rca_drill_2,
        )
    };

    println!("bd: {}", bd);

    // now do the final join

    let mut final_sql = format!("select * from ({}) all inner join ({}) using {}",
        ac,
        bd,
        b_final_drills,
    );


    // adding final measures at the end
    let final_ext_meas = if !meas.is_empty() {
        ", ".to_owned() + &join((1..meas.len()+1).map(|i| format!("m{}", i)), ", ")
    } else {
        "".to_owned()
    };

    final_sql = format!("select {}, ((a/b) / (c/d)) as rca{} from ({})",
        a_final_drills,
        final_ext_meas,
        final_sql,
    );

    // SPECIAL CASE
    // Hack to deal with no drills on d
    // Later, make this better
    final_sql = final_sql.replace("select , ", "select ");
    final_sql = final_sql.replace("group by )", ")");


    (final_sql, a_final_drills)
}

#[cfg(test)]
mod test {
    use super::*;
    use super::super::*;

    #[test]
    fn test_rca_sql() {
        let table = TableSql {
            name: "sales".into(),
            primary_key: Some("product_id".into()),
        };
        //let cuts = vec![
        //    CutSql {
        //        foreign_key: "product_id".into(),
        //        primary_key: "product_id".into(),
        //        table: Table { name: "dim_products".into(), schema: None, primary_key: None },
        //        column: "product_group_id".into(),
        //        members: vec!["3".into()],
        //        member_type: MemberType::NonText,
        //    },
        //];
        //let drills = vec![
        //    // this dim is inline, so should use the fact table
        //    // also has parents, so has 
        //    DrilldownSql {
        //        foreign_key: "date_id".into(),
        //        primary_key: "date_id".into(),
        //        table: Table { name: "sales".into(), schema: None, primary_key: None },
        //        level_columns: vec![
        //            LevelColumn {
        //                key_column: "year".into(),
        //                name_column: None,
        //            },
        //            LevelColumn {
        //                key_column: "month".into(),
        //                name_column: None,
        //            },
        //            LevelColumn {
        //                key_column: "day".into(),
        //                name_column: None,
        //            },
        //        ],
        //        property_columns: vec![],
        //    },
        //    // this comes second, but should join first because of primary key match
        //    // on fact table
        //    DrilldownSql {
        //        foreign_key: "product_id".into(),
        //        primary_key: "product_id".into(),
        //        table: Table { name: "dim_products".into(), schema: None, primary_key: None },
        //        level_columns: vec![
        //            LevelColumn {
        //                key_column: "product_group_id".into(),
        //                name_column: Some("product_group_label".into()),
        //            },
        //            LevelColumn {
        //                key_column: "product_id_raw".into(),
        //                name_column: Some("product_label".into()),
        //            },
        //        ],
        //        property_columns: vec![],
        //    },
        //];
        //let meas = vec![
        //    MeasureSql { aggregator: "sum".into(), column: "quantity".into() }
        //];

        let drill_1 = vec![DrilldownSql {
            foreign_key: "date_id".into(),
            primary_key: "date_id".into(),
            table: Table { name: "sales".into(), schema: None, primary_key: None },
            level_columns: vec![
                LevelColumn {
                    key_column: "year".into(),
                    name_column: None,
                },
                LevelColumn {
                    key_column: "month".into(),
                    name_column: None,
                },
                LevelColumn {
                    key_column: "day".into(),
                    name_column: None,
                },
            ],
            property_columns: vec![],
        }];

        let drill_2 = vec![DrilldownSql {
            foreign_key: "product_id".into(),
            primary_key: "product_id".into(),
            table: Table { name: "dim_products".into(), schema: None, primary_key: None },
            level_columns: vec![
                LevelColumn {
                    key_column: "product_group_id".into(),
                    name_column: Some("product_group_label".into()),
                },
                LevelColumn {
                    key_column: "product_id_raw".into(),
                    name_column: Some("product_label".into()),
                },
            ],
            property_columns: vec![],
        }];

        let mea = MeasureSql { aggregator: "sum".into(), column: "quantity".into() };

        let rca = RcaSql {
            drill_1,
            drill_2,
            mea,
        };

        assert_eq!(
            clickhouse_sql(&table, &[], &[], &[], &None, &None, &None, &Some(rca), &None),
            "".to_owned()
        );
    }
}